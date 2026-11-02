//! Finds Helium's DevTools endpoint and proves the listening socket belongs to that Helium.
//!
//! The browser process is the Helium using the configured user data directory. Its port is the
//! `--remote-debugging-port` it was started with; for an ephemeral port (0) Helium writes the
//! chosen port to `DevToolsActivePort`, which it does not do for a fixed one.

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::Config;

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub ws_url: String,
    /// Browser instance id: the last path segment of the WebSocket URL.
    pub browser_id: String,
    /// PID of the Helium browser process that owns the socket (`None` for `HBA_CDP`).
    pub pid: Option<u32>,
}

/// The verified endpoint of the Helium using the configured user data directory.
pub fn discover(config: &Config) -> Result<Endpoint> {
    if let Some(url) = &config.cdp_url {
        return Ok(Endpoint { browser_id: id_of(url).to_owned(), ws_url: url.clone(), pid: None });
    }
    let pid = browser_pid(config)
        .with_context(|| format!("Helium is not running with user data dir {}", config.user_data_dir.display()))?;
    let (port, path) = match flag_port(pid) {
        Some(port) if port > 0 => (port, None),
        flag => {
            let file = config.user_data_dir.join("DevToolsActivePort");
            let text = fs::read_to_string(&file).map_err(|_| match flag {
                None => anyhow!(
                    "Helium runs without remote debugging; add --remote-debugging-port=9222 to \
                     ~/.config/helium-browser-flags.conf and restart Helium"
                ),
                Some(_) => anyhow!("Helium has not published its DevTools port ({} is missing)", file.display()),
            })?;
            let (port, path) = parse_active_port(&text).with_context(|| format!("invalid {}", file.display()))?;
            (port, Some(path.to_owned()))
        }
    };
    let inodes = listening_inodes(port)?;
    if !owns_sockets(pid, &inodes) {
        bail!("the DevTools port {port} is not owned by Helium (pid {pid}); refusing to connect");
    }
    let ws_url = match path {
        Some(path) => format!("ws://127.0.0.1:{port}{path}"),
        None => version_url(port)?,
    };
    Ok(Endpoint { browser_id: id_of(&ws_url).to_owned(), ws_url, pid: Some(pid) })
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

/// Inodes of the sockets listening on loopback `port`; refuses the port when any listener on it
/// is a wildcard or public one.
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
    if inodes.is_empty() {
        bail!("nothing listens on 127.0.0.1:{port}; Helium may have exited (stale DevToolsActivePort)");
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

/// The Helium browser process holding the configured user data dir, from the `SingletonLock`
/// link (`HOST-PID`) that Chromium keeps there while it runs.
fn browser_pid(config: &Config) -> Option<u32> {
    let link = fs::read_link(config.user_data_dir.join("SingletonLock")).ok()?;
    let (host, pid) = link.to_str()?.rsplit_once('-')?;
    let pid = pid.parse().ok()?;
    let local = fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    (host == local.trim() && is_browser(pid)).then_some(pid)
}

/// A Helium browser process, not one of its children.
fn is_browser(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.trim_end() == "helium")
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

/// True when a Helium browser process holds this user data dir.
pub fn running(config: &Config) -> bool {
    browser_pid(config).is_some()
}

/// Starts the launcher detached, e.g. to open a profile window or start Helium. The process is
/// reaped when it exits, so a Helium hba started can be seen to quit.
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

/// Starts Helium when it is not running and waits for a verified endpoint.
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
                return Err(error.context("Helium started but its DevTools endpoint did not appear within 20s"));
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
}
