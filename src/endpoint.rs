//! Finds the browser's DevTools endpoint and proves the listening socket belongs to that browser.
//!
//! The browser process is the one of the configured kind using the configured user data
//! directory. Its port is the `--remote-debugging-port` it was started with. The browser writes
//! the port to `DevToolsActivePort` for an ephemeral port (0), and when remote debugging was
//! turned on in its settings instead (the switch at `chrome://inspect/#remote-debugging`): then
//! it asks its user to allow each connection. It writes no file for a fixed port.

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{BrowserKind, Config};

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub ws_url: String,
    /// Browser instance id: the last path segment of the WebSocket URL.
    pub browser_id: String,
    /// PID of the browser process that owns the socket (`None` for `HBA_CDP`).
    pub pid: Option<u32>,
    /// The browser asks its user to allow each connection (its remote debugging switch, no flag).
    pub approval: bool,
}

/// Where a browser process's DevTools endpoint comes from.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// `--remote-debugging-port=N`: the URL comes from `/json/version`.
    Fixed(u16),
    /// `DevToolsActivePort`, written for port 0 and by the remote debugging switch, which needs
    /// the user's approval for each connection.
    PortFile { approval: bool },
    /// A flag the browser ignores; it then starts no DevTools server, not even the switch's.
    Ignored,
}

/// The plan for a browser started with `flag` as its `--remote-debugging-port`.
fn plan(flag: Option<u16>, ignores_flag: bool) -> Plan {
    match flag {
        Some(_) if ignores_flag => Plan::Ignored,
        Some(0) => Plan::PortFile { approval: false },
        Some(port) => Plan::Fixed(port),
        None => Plan::PortFile { approval: true },
    }
}

/// How to turn on the remote debugging switch, with which the browser asks before each connection.
fn switch_hint(config: &Config) -> String {
    format!(
        "turn on \"Allow remote debugging for this browser instance\" at {} and click Allow when {} asks",
        config.browser.inspect_page(),
        config.browser.name()
    )
}

/// Where the `--remote-debugging-port` flag goes for the configured browser and data dir: the
/// browser's flags file, except for Chrome, whose flags file reaches its default profile too,
/// where the flag turns remote debugging off. `None` where the flag cannot work.
pub fn flag_hint(config: &Config) -> Option<String> {
    let name = config.browser.name();
    match config.browser {
        _ if config.ignores_port_flag() => None,
        BrowserKind::Chrome => Some(format!(
            "start {name} with --user-data-dir={} --remote-debugging-port=9222",
            config.user_data_dir.display()
        )),
        BrowserKind::Helium | BrowserKind::Brave => {
            Some(format!("add --remote-debugging-port=9222 to {} and restart {name}", config.flags_file().display()))
        }
    }
}

/// How to turn on remote debugging for the configured browser.
fn how_to_enable(config: &Config) -> String {
    match flag_hint(config) {
        Some(flag) => format!("{flag}, or {}", switch_hint(config)),
        None => switch_hint(config),
    }
}

/// The verified endpoint of the browser using the configured user data directory.
pub fn discover(config: &Config) -> Result<Endpoint> {
    if let Some(url) = &config.cdp_url {
        return Ok(Endpoint { browser_id: id_of(url).to_owned(), ws_url: url.clone(), pid: None, approval: false });
    }
    let name = config.browser.name();
    let pid = browser_pid(config)
        .with_context(|| format!("{name} is not running with user data dir {}", config.user_data_dir.display()))?;
    let (port, path, approval) = match plan(flag_port(pid), config.ignores_port_flag()) {
        Plan::Fixed(port) => (port, None, false),
        Plan::Ignored => bail!(
            "{name} ignores --remote-debugging-port on its default user data dir and then keeps remote debugging off; \
             remove the flag (also from {}), restart {name}, then {}",
            config.flags_file().display(),
            switch_hint(config)
        ),
        Plan::PortFile { approval } => {
            let file = config.user_data_dir.join("DevToolsActivePort");
            let text = fs::read_to_string(&file).map_err(|_| {
                if approval {
                    anyhow!("{name} runs without remote debugging; {}", how_to_enable(config))
                } else {
                    anyhow!("{name} has not published its DevTools port ({} is missing)", file.display())
                }
            })?;
            let (port, path) = parse_active_port(&text).with_context(|| format!("invalid {}", file.display()))?;
            (port, Some(path.to_owned()), approval)
        }
    };
    let inodes = listening_inodes(port)?;
    if inodes.is_empty() {
        if approval {
            bail!(
                "nothing listens on 127.0.0.1:{port} (stale DevToolsActivePort): {name} has remote debugging off; {}",
                how_to_enable(config)
            );
        }
        bail!("nothing listens on 127.0.0.1:{port}; {name} may have exited (stale DevToolsActivePort)");
    }
    if !owns_sockets(pid, &inodes) {
        bail!("the DevTools port {port} is not owned by {name} (pid {pid}); refusing to connect");
    }
    let ws_url = match path {
        Some(path) => format!("ws://127.0.0.1:{port}{path}"),
        None => version_url(port)?,
    };
    Ok(Endpoint { browser_id: id_of(&ws_url).to_owned(), ws_url, pid: Some(pid), approval })
}

/// The `--remote-debugging-port` a process was started with; the last one counts, as in Chromium.
fn flag_port(pid: u32) -> Option<u16> {
    switches(pid).iter().rev().find_map(|arg| arg.strip_prefix("--remote-debugging-port="))?.parse().ok()
}

/// The browser WebSocket URL from `/json/version` on a verified port. The daemon runs on one
/// thread, so the whole exchange is bounded.
fn version_url(port: u16) -> Result<String> {
    const LIMIT: Duration = Duration::from_secs(1);
    let fail = || format!("reading the DevTools version endpoint on port {port}");
    let deadline = Instant::now() + LIMIT;
    let left = || match deadline.checked_duration_since(Instant::now()) {
        Some(left) if !left.is_zero() => Ok(left),
        _ => Err(anyhow!("{}: no answer within {}s", fail(), LIMIT.as_secs())),
    };
    let mut stream =
        TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), LIMIT).with_context(fail)?;
    stream.set_write_timeout(Some(left()?)).with_context(fail)?;
    stream
        .write_all(
            format!("GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .with_context(fail)?;
    // The DevTools server may keep the connection open, so read exactly Content-Length bytes.
    let mut response = Vec::new();
    let mut chunk = [0_u8; 4096];
    let body = loop {
        stream.set_read_timeout(Some(left()?)).with_context(fail)?;
        let read = stream.read(&mut chunk).with_context(fail)?;
        if read == 0 {
            bail!("{}: the connection closed early", fail());
        }
        response.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&response);
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("content-length")))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .with_context(fail)?;
            if body.len() >= length {
                break body[..length].to_owned();
            }
        }
    };
    let version: serde_json::Value = serde_json::from_str(&body).with_context(fail)?;
    let url = version["webSocketDebuggerUrl"].as_str().with_context(fail)?;
    let path = url
        .strip_prefix(&format!("ws://127.0.0.1:{port}"))
        .with_context(|| format!("unexpected DevTools URL {url}"))?;
    parse_active_port(&format!("{port}\n{path}")).with_context(|| format!("unexpected DevTools URL {url}"))?;
    Ok(url.to_owned())
}

fn id_of(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

fn parse_active_port(text: &str) -> Result<(u16, &str)> {
    let mut lines = text.lines();
    let port: u16 = lines.next().unwrap_or_default().trim().parse().context("bad port line")?;
    let path = lines.next().unwrap_or_default().trim();
    let id = path.strip_prefix("/devtools/browser/").context("bad browser path")?;
    if port == 0 || id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        bail!("bad endpoint");
    }
    Ok((port, path))
}

/// Inodes of the sockets listening on loopback `port` (none when nothing listens); refuses the
/// port when any listener on it is a wildcard or public one.
fn listening_inodes(port: u16) -> Result<Vec<u64>> {
    let wanted = format!("{port:04X}");
    let mut inodes = Vec::new();
    let mut exposed = false;
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (Some(local), Some(&"0A"), Some(inode)) = (fields.get(1), fields.get(3), fields.get(9)) else {
                continue;
            };
            let Some((address, local_port)) = local.split_once(':') else { continue };
            if local_port != wanted {
                continue;
            }
            match address {
                "0100007F" | "00000000000000000000000001000000" | "0000000000000000FFFF00000100007F" => {
                    inodes.push(inode.parse().context("bad inode")?);
                }
                _ => exposed = true,
            }
        }
    }
    if exposed {
        bail!("port {port} listens beyond loopback; refusing to drive a browser exposed to the network");
    }
    Ok(inodes)
}

/// Whether process `pid` holds every socket in `inodes`.
fn owns_sockets(pid: u32, inodes: &[u64]) -> bool {
    let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else { return false };
    let held: Vec<_> = fds.flatten().filter_map(|fd| fs::read_link(fd.path()).ok()).collect();
    inodes.iter().all(|inode| {
        let target = format!("socket:[{inode}]");
        held.iter().any(|link| link.as_os_str() == target.as_str())
    })
}

/// The browser process holding the configured user data dir, from the `SingletonLock` link
/// (`HOST-PID`) that Chromium keeps there while it runs.
fn browser_pid(config: &Config) -> Option<u32> {
    let link = fs::read_link(config.user_data_dir.join("SingletonLock")).ok()?;
    let (host, pid) = link.to_str()?.rsplit_once('-')?;
    let pid = pid.parse().ok()?;
    let local = fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    (host == local.trim() && is_browser(pid, config.browser.process())).then_some(pid)
}

/// A browser process named `process`, not one of its children.
fn is_browser(pid: u32, process: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.trim_end() == process)
        && !switches(pid).iter().any(|arg| arg.starts_with("--type="))
}

/// Command-line switches of a process. Chromium rewrites its arguments into one
/// space-separated process title, so they are split on spaces as well.
fn switches(pid: u32) -> Vec<String> {
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else { return Vec::new() };
    String::from_utf8_lossy(&cmdline)
        .split(['\0', ' '])
        .filter(|arg| arg.starts_with("--"))
        .map(str::to_owned)
        .collect()
}

/// True when a browser process holds this user data dir.
pub fn running(config: &Config) -> bool {
    browser_pid(config).is_some()
}

/// Whether the remote debugging switch is on in the data dir's `Local State`, as the browser
/// last saved it.
pub fn switch_on(config: &Config) -> bool {
    let Ok(text) = fs::read(config.user_data_dir.join("Local State")) else { return false };
    serde_json::from_slice::<serde_json::Value>(&text)
        .ok()
        .and_then(|state| state.pointer("/devtools/remote_debugging/user-enabled")?.as_bool())
        .unwrap_or(false)
}

/// Starts the launcher detached, e.g. to open a profile window or start the browser. The
/// process is reaped when it exits, so a browser hba started can be seen to quit.
pub fn launch(config: &Config, args: &[String]) -> Result<()> {
    let mut child = std::process::Command::new(&config.launcher)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {}", config.launcher.display()))?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Starts the browser when it is not running and waits for a verified endpoint.
pub async fn ensure_running(config: &Config, profile_dir: Option<&str>) -> Result<Endpoint> {
    if let Ok(endpoint) = discover(config) {
        return Ok(endpoint);
    }
    if config.cdp_url.is_some() || running(config) {
        // A running browser whose endpoint fails verification is never second-guessed.
        return discover(config);
    }
    let mut args = Vec::new();
    if let Some(dir) = profile_dir {
        args.push(format!("--profile-directory={dir}"));
    }
    launch(config, &args)?;
    // Verification rejects anything left over from an earlier run (a stale port file or port).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match discover(config) {
            Ok(endpoint) => return Ok(endpoint),
            Err(error) if tokio::time::Instant::now() >= deadline => {
                let name = config.browser.name();
                return Err(
                    error.context(format!("{name} started but its DevTools endpoint did not appear within 20s"))
                );
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_port_file_is_validated() {
        assert_eq!(parse_active_port("9222\n/devtools/browser/ab-12\n").unwrap(), (9222, "/devtools/browser/ab-12"));
        assert!(parse_active_port("0\n/devtools/browser/x").is_err());
        assert!(parse_active_port("9222\n/json/version").is_err());
        assert!(parse_active_port("9222\n/devtools/browser/a\"b").is_err());
    }

    #[test]
    fn the_flag_decides_where_the_endpoint_comes_from() {
        assert_eq!(plan(Some(9222), false), Plan::Fixed(9222));
        assert_eq!(plan(Some(0), false), Plan::PortFile { approval: false });
        // No flag: only the remote debugging switch can have started a server, which asks first.
        assert_eq!(plan(None, false), Plan::PortFile { approval: true });
        assert_eq!(plan(None, true), Plan::PortFile { approval: true });
        // Chrome on its default data dir ignores the flag and starts no server at all.
        assert_eq!(plan(Some(9222), true), Plan::Ignored);
        assert_eq!(plan(Some(0), true), Plan::Ignored);
    }
}
