//! Browser lifecycle, the daemon itself, and diagnostics.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::cdp::Cdp;
use crate::cli::{BrowserOp, DaemonOp};
use crate::config::Config;
use crate::daemon::Cx;
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

/// Quits Helium and relaunches every profile that was open, restoring their tabs.
async fn restart(cx: &mut Cx<'_>) -> Result<()> {
    let config = cx.config()?;
    let browser = cx.browser().await?;
    let pid = browser.endpoint.pid.context("restart needs a verified local Helium")?;
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
    // A task of its own: a client that hangs up cancels the command, which must not leave
    // Helium quit.
    tokio::spawn(relaunch(browser.cdp.clone(), config, pid, dirs)).await.context("restarting Helium")??;
    let browser = cx.browser().await?;
    cx.say(format!("restarted {} with {}", browser.product, crate::output::count(count, "profile")));
    Ok(())
}

/// Quits the browser `pid` and launches it again with the profile directories `dirs`.
async fn relaunch(cdp: Cdp, config: Arc<Config>, pid: u32, dirs: Vec<String>) -> Result<()> {
    cdp.send("Browser.close", json!({}), None).await.ok();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        if tokio::time::Instant::now() >= deadline {
            bail!("Helium did not exit; a dialog (e.g. unsaved form) may be blocking it");
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
                    bail!("Helium restarted but its DevTools endpoint did not come back");
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
            match cx.hub.connected().await {
                Some(browser) => cx.say(format!("connected to {} at {}", browser.product, browser.endpoint.ws_url)),
                None => cx.say("not connected to a browser"),
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
            cx.say("daemon stopping; tabs stay open");
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
    cx.say(format!("✓ hba {} (daemon pid {})", crate::ipc::BUILD, std::process::id()));
    cx.say(format!("· user data dir {}", config.user_data_dir.display()));
    match crate::profiles::Profiles::load(&config.user_data_dir) {
        Ok(profiles) => cx.say(format!(
            "✓ {} profiles: {}",
            profiles.list.len(),
            profiles.list.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
        Err(error) => cx.say(format!("✗ profiles: {error:#}")),
    }
    let flags = crate::config::xdg("XDG_CONFIG_HOME", ".config").join("helium-browser-flags.conf");
    let port_flag = std::fs::read_to_string(&flags).ok().and_then(|text| {
        text.lines().filter(|line| !line.trim_start().starts_with('#')).find_map(|line| {
            line.split_whitespace().find_map(|word| word.strip_prefix("--remote-debugging-port=").map(str::to_owned))
        })
    });
    match port_flag.as_deref() {
        Some("0") => cx.say(format!(
            "! {} uses port 0, which makes navigator.webdriver true; use a fixed port such as 9222",
            flags.display()
        )),
        Some(port) => cx.say(format!("✓ remote debugging port {port} ({})", flags.display())),
        None => cx.say(format!("! no --remote-debugging-port in {}; hba needs it", flags.display())),
    }
    match endpoint::discover(&config) {
        Ok(found) => {
            let owner = found.pid.map(|pid| format!(", owned by Helium pid {pid}")).unwrap_or_default();
            cx.say(format!("✓ endpoint verified: loopback only{owner}"));
            match cx.browser().await {
                Ok(browser) => cx.say(format!("✓ connected: {}", browser.product)),
                Err(error) => cx.say(format!("✗ connect: {error:#}")),
            }
        }
        Err(error) if endpoint::running(&config) => cx.say(format!("✗ endpoint: {error:#}")),
        Err(_) => cx.say("· Helium is not running; `hba open URL` or `hba browser launch` starts it"),
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
