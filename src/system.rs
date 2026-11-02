//! Browser lifecycle, the daemon itself, and diagnostics.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::cdp::Cdp;
use crate::cli::{BrowserOp, DaemonOp};
use crate::config::{BrowserKind, Config};
use crate::daemon::{ApprovalRequest, Cx};
use crate::endpoint;

pub async fn browser(cx: &mut Cx<'_>, op: BrowserOp) -> Result<()> {
    match op {
        BrowserOp::Launch => {
            let browser = cx.browser().await?;
            let pid = browser.endpoint.pid.map(|pid| format!(" pid {pid}")).unwrap_or_default();
            cx.say(format!("{}{pid}", browser.product));
        }
        BrowserOp::Endpoint => {
            let endpoint = endpoint::discover(&*cx.config()?)?;
            cx.say(endpoint.ws_url);
        }
        BrowserOp::Restart => restart(cx).await?,
    }
    Ok(())
}

/// Quits the browser and relaunches every profile that was open, restoring their tabs.
async fn restart(cx: &mut Cx<'_>) -> Result<()> {
    let config = cx.config()?;
    let name = config.browser.name();
    let browser = cx.browser().await?;
    let pid = browser.endpoint.pid.with_context(|| format!("restart needs a verified local {name}"))?;
    let profiles = browser.profiles()?;
    let mut dirs: Vec<String> = browser.contexts().into_values().collect();
    for dir in &profiles.open {
        if !dirs.contains(dir) {
            dirs.push(dir.clone());
        }
    }
    if dirs.is_empty() {
        dirs.push(profiles.last_used.clone().unwrap_or_else(|| "Default".to_owned()));
    }
    let count = dirs.len();
    // A task of its own: a client that hangs up cancels the command, which must not leave the
    // browser quit.
    tokio::spawn(relaunch(browser.cdp.clone(), config, pid, dirs))
        .await
        .with_context(|| format!("restarting {name}"))??;
    let browser = cx.browser().await?;
    cx.say(format!("restarted {} with {}", browser.product, crate::output::count(count, "profile")));
    Ok(())
}

/// Quits the browser `pid` and launches it again with the profile directories `dirs`.
async fn relaunch(cdp: Cdp, config: Arc<Config>, pid: u32, dirs: Vec<String>) -> Result<()> {
    let name = config.browser.name();
    cdp.send("Browser.close", json!({}), None).await.ok();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        if tokio::time::Instant::now() >= deadline {
            bail!("{name} did not exit; a dialog (e.g. unsaved form) may be blocking it");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    for (index, dir) in dirs.iter().enumerate() {
        endpoint::launch(&config, &[format!("--profile-directory={dir}"), "--restore-last-session".to_owned()])?;
        if index == 0 {
            // The first launch starts the browser; later ones reach it through its singleton.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            loop {
                // Discovery reads /proc and may wait a second for `/json/version`: off the runtime thread.
                let config = Arc::clone(&config);
                if tokio::task::spawn_blocking(move || endpoint::discover(&config).is_ok()).await? {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    bail!("{name} restarted but its DevTools endpoint did not come back");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    Ok(())
}

pub async fn daemon(cx: &mut Cx<'_>, op: DaemonOp) -> Result<()> {
    match op {
        DaemonOp::Status => {
            let uptime = cx.hub.started.elapsed().as_secs();
            cx.say(format!("hba {} pid {} up {}m{}s", crate::ipc::BUILD, std::process::id(), uptime / 60, uptime % 60));
            let name = cx.config().map_or("the browser", |config| config.browser.name());
            match (cx.hub.connected().await, cx.hub.approval_request()) {
                (Some(browser), _) => {
                    cx.say(format!("connected to {} at {}", browser.product, browser.endpoint.ws_url));
                }
                (None, Some(ApprovalRequest::Waiting { behind })) => {
                    let note = if behind { crate::daemon::abandoned_note(name) } else { String::new() };
                    cx.say(format!("not connected: waiting for Allow in {name}'s dialog (approval mode){note}"));
                }
                (None, Some(ApprovalRequest::Allowed)) => {
                    cx.say(format!("not connected yet: {name} allowed the connection, which the next command takes"));
                }
                (None, None) => cx.say("not connected to a browser"),
            }
            let sessions: Vec<String> = {
                let store = cx.hub.store.lock().expect("store lock");
                store.sessions.iter().map(|(name, s)| format!("{name} ({} tabs)", s.tabs.len())).collect()
            };
            if !sessions.is_empty() {
                cx.say(format!("sessions: {}", sessions.join(", ")));
            }
        }
        DaemonOp::Stop => {
            if matches!(cx.hub.approval_request(), Some(ApprovalRequest::Waiting { .. })) {
                // Its dialog would stay in front of the next daemon's and connect nothing.
                let name = cx.config().map_or("the browser", |config| config.browser.name());
                cx.say(format!(
                    "daemon stopping once you answer {name}'s \"Allow remote debugging?\" dialog (at most {}s); tabs \
                     stay open",
                    crate::ipc::DAEMON_DRAIN.as_secs()
                ));
            } else {
                cx.say("daemon stopping; tabs stay open");
            }
            let hub = Arc::clone(cx.hub);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                hub.stop.notify_one();
            });
        }
    }
    Ok(())
}

pub async fn doctor(cx: &mut Cx<'_>) -> Result<()> {
    let config = match cx.config() {
        Ok(config) => config,
        Err(error) => {
            cx.say(format!("✗ config: {error:#}"));
            return Ok(());
        }
    };
    let name = config.browser.name();
    cx.say(format!("✓ hba {} (daemon pid {})", crate::ipc::BUILD, std::process::id()));
    cx.say(format!("· {name}, user data dir {}", config.user_data_dir.display()));
    match crate::profiles::Profiles::load(&config.user_data_dir) {
        Ok(profiles) => cx.say(format!(
            "✓ {} profiles: {}",
            profiles.list.len(),
            profiles.list.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
        Err(error) => cx.say(format!("✗ profiles: {error:#}")),
    }
    let flags = config.flags_file();
    let port_flag = std::fs::read_to_string(&flags).ok().and_then(|text| {
        text.lines().filter(|line| !line.trim_start().starts_with('#')).find_map(|line| {
            line.split_whitespace().find_map(|word| word.strip_prefix("--remote-debugging-port=").map(str::to_owned))
        })
    });
    let inspect = config.browser.inspect_page();
    let found = endpoint::discover(&config);
    // A browser in approval mode has the switch on, even before it saved that to `Local State`.
    let switch = endpoint::switch_on(&config) || found.as_ref().is_ok_and(|endpoint| endpoint.approval);
    // Started with the flag on its own command line rather than from the flags file.
    let by_flag = found.as_ref().is_ok_and(|endpoint| endpoint.pid.is_some() && !endpoint.approval);
    // The flag needs no approval and wins over the switch. Chrome ignores it on its default data
    // dir, and Chrome's flags file reaches that dir whatever `user_data_dir` says.
    match (port_flag.as_deref(), endpoint::flag_hint(&config)) {
        (Some(_), None) => cx.say(format!(
            "✗ {} sets --remote-debugging-port, which {name} ignores on its default user data dir and which keeps \
             remote debugging off there; remove it and use the switch at {inspect}",
            flags.display()
        )),
        (Some(_), Some(flag)) if config.browser == BrowserKind::Chrome => cx.say(format!(
            "! {} sets --remote-debugging-port for every {name} start, and {name}'s default profile then keeps \
             remote debugging off; remove it and {flag} instead",
            flags.display()
        )),
        (Some("0"), _) => cx.say(format!(
            "! {} uses port 0, which makes navigator.webdriver true; use a fixed port such as 9222",
            flags.display()
        )),
        (Some(port), _) => cx.say(format!("✓ remote debugging port {port} ({})", flags.display())),
        (None, _) if by_flag => {}
        (None, None) if switch => cx.say(format!(
            "✓ approval mode: the remote debugging switch at {inspect} is on; {name} asks you to allow each new \
             connection (the only way on its default user data dir)"
        )),
        (None, None) => cx.say(format!(
            "! remote debugging is off: on its default user data dir {name} takes connections only through the \
             switch at {inspect}; turn it on"
        )),
        (None, Some(flag)) if switch => cx.say(format!(
            "! approval mode: the switch at {inspect} is on, so {name} asks you to allow each new connection; to \
             need no approval (recommended), {flag}"
        )),
        (None, Some(flag)) => cx
            .say(format!("! remote debugging is not set up: {flag} (recommended), or turn on the switch at {inspect}")),
    }
    match found {
        Ok(found) => {
            let owner = found.pid.map(|pid| format!(", owned by {name} pid {pid}")).unwrap_or_default();
            let mode = match (found.approval, found.pid) {
                (true, _) => format!("; approval mode: {name} asks you to allow each new connection"),
                (false, Some(_)) => "; --remote-debugging-port, no approval needed".to_owned(),
                (false, None) => String::new(),
            };
            cx.say(format!("✓ endpoint verified: loopback only{owner}{mode}"));
            match cx.browser().await {
                Ok(browser) => cx.say(format!("✓ connected: {}", browser.product)),
                Err(error) => cx.say(format!("✗ connect: {error:#}")),
            }
        }
        Err(error) if endpoint::running(&config) => cx.say(format!("✗ endpoint: {error:#}")),
        Err(_) => cx.say(format!("· {name} is not running; `hba open URL` or `hba browser launch` starts it")),
    }
    let launcher = which(&config.launcher);
    match launcher {
        Some(path) => cx.say(format!("✓ launcher {}", path.display())),
        None => cx.say(format!("✗ launcher {} not found (set `launcher` in the config)", config.launcher.display())),
    }
    Ok(())
}

fn which(program: &std::path::Path) -> Option<std::path::PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_owned());
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| std::path::Path::new(dir).join(program))
        .find(|path| path.is_file())
}
