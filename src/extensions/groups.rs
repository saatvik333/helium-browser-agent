//! Agent tabs in one tab group per window, titled `hba`, so the user sees where agents work.
//! The DevTools protocol cannot group tabs; only the extension API can, so hba ships a small
//! extension (`assets/extension`) and calls into its service worker.
//!
//! `Extensions.loadUnpacked` installs into the browser's last-used profile, and only until the
//! browser restarts, so hba loads it when a tab's profile is that one. Other profiles need it
//! loaded once by hand from [`dir`], which the browser then keeps. A profile counts as having
//! the helper only once its service worker runs in the tab's browser context.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;

use super::catalog::{self, Loaded};
use super::debugger::{self, Kind};
use crate::browser::{Browser, TargetInfo};
use crate::page::Page;

const MANIFEST: &str = include_str!("../../assets/extension/manifest.json");
const WORKER: &str = include_str!("../../assets/extension/worker.js");

/// The title of the agents' tab group.
const TITLE: &str = "hba";

/// The extension's id: the manifest's `key` fixes it in every profile and directory (the first
/// 32 hex digits of the key's SHA-256, written with `a`–`p`).
const ID: &str = "nocpjggjieaipppbhonnamkffkappnmn";

/// Longest wait for the extension's service worker to start.
const WORKER_WAIT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);

/// Profiles (by directory) found without the helper, for one browser connection, with the
/// modification time of their `Preferences` then: they are looked at again once it changes, as
/// loading the helper by hand changes it. `None`: not again on this connection.
pub type Missing = HashMap<String, Option<SystemTime>>;

/// Where the extension's files are written, to load over DevTools or by hand.
pub fn dir() -> PathBuf {
    crate::config::xdg("XDG_DATA_HOME", ".local/share").join("hba/extension")
}

/// Moves the tabs of `pages` (page target ids) into the `hba` group in their windows. `via` is
/// an attached page in the same profile, used to start the extension's stopped service worker.
/// Returns what the user should know: the profile lacks the extension, the first time it does.
pub async fn add(browser: &Arc<Browser>, via: &Page, pages: &[String]) -> Result<Option<String>> {
    if !browser.config().tab_groups {
        return Ok(None);
    }
    let Some(context) = browser.tab_info(&via.tab).await?.and_then(|tab| tab.browser_context_id) else {
        return Ok(None);
    };
    let Some(profile) = browser.profile_of_context(&context) else { return Ok(None) };
    let worker = if let Some(worker) = worker(browser, &context).await? {
        worker
    } else {
        let known = browser.tab_groups.lock().expect("tab groups lock").get(&profile).copied();
        let prefs = browser.config().user_data_dir.join(&profile).join("Preferences");
        let modified = std::fs::metadata(&prefs).and_then(|meta| meta.modified()).ok();
        if known.is_some_and(|at| at.is_none() || at == modified) {
            return Ok(None);
        }
        let Some(worker) = find(browser, via, &context, &profile).await? else {
            browser.tab_groups.lock().expect("tab groups lock").insert(profile.clone(), modified);
            return if known.is_some() { Ok(None) } else { hint(browser, &profile).map(Some) };
        };
        browser.tab_groups.lock().expect("tab groups lock").remove(&profile);
        worker
    };
    let lease = debugger::single_use(browser, &worker, Kind::Worker).await?;
    let code = format!("group({}, {})", serde_json::to_string(pages)?, serde_json::to_string(TITLE)?);
    let grouped = crate::page::evaluate_on(&browser.cdp, &lease.session, None, &code, false, WORKER_WAIT).await;
    lease.end().await;
    grouped.context("grouping the tab")?;
    Ok(None)
}

/// The helper's service worker in `context`, started when `profile` records the helper, or
/// loaded first when `profile` is the browser's last-used one. `None` when it does not start
/// there: not installed, or installed into the profile the browser used last since `Local State`
/// was written.
async fn find(browser: &Browser, via: &Page, context: &str, profile: &str) -> Result<Option<TargetInfo>> {
    if !has_extension(browser, profile).await? {
        if browser.profiles()?.default_dir() != profile {
            return Ok(None);
        }
        if let Err(error) = load(browser).await {
            // A browser that refuses the Extensions domain refuses it for the whole connection.
            browser.tab_groups.lock().expect("tab groups lock").insert(profile.to_owned(), None);
            return Err(error);
        }
    }
    wake(browser, via, context).await
}

/// How to install the helper in `profile` by hand.
fn hint(browser: &Browser, profile: &str) -> Result<String> {
    write_files(&dir())?;
    let config = browser.config();
    let profiles = browser.profiles()?;
    let name = profiles.by_dir(profile).map_or(profile, |found| found.name.as_str());
    Ok(format!(
        "{} installs hba's extension over DevTools only in the profile used last; to group tabs \
         in {name:?} too, load {} there once at {}://extensions (developer mode, Load unpacked), or set \
         tab_groups = false",
        config.browser.name(),
        super::tilde(&dir()),
        config.browser.scheme()
    ))
}

/// Loads the extension into the last-used profile unless it has it already.
async fn load(browser: &Browser) -> Result<()> {
    #[derive(Deserialize)]
    struct List {
        extensions: Vec<Loaded>,
    }
    let list: List = browser.cdp.call("Extensions.getExtensions", json!({}), None).await?;
    if let Some(loaded) = list.extensions.iter().find(|loaded| loaded.id == ID) {
        // Turned off by the user, who then does not want it.
        anyhow::ensure!(loaded.enabled, "hba's extension {ID} is turned off");
        return Ok(());
    }
    let dir = dir();
    write_files(&dir)?;
    let path = dir.to_str().context("the data directory path is not valid UTF-8")?;
    browser.cdp.send("Extensions.loadUnpacked", json!({"path": path}), None).await
}

/// Whether a profile's preferences record the extension, enabled: loaded by hand, or over
/// DevTools since the browser started.
async fn has_extension(browser: &Browser, profile: &str) -> Result<bool> {
    // Preferences files run to megabytes; reading and parsing them stays off the runtime thread.
    let profile_dir = browser.config().user_data_dir.join(profile);
    let list = tokio::task::spawn_blocking(move || catalog::installed(&profile_dir)).await??;
    Ok(list.iter().any(|ext| ext.id == ID && ext.enabled))
}

/// Writes the extension's files where they differ, as after an upgrade of hba.
fn write_files(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, text) in [("manifest.json", MANIFEST), ("worker.js", WORKER)] {
        let path = dir.join(name);
        if std::fs::read(&path).ok().as_deref() != Some(text.as_bytes()) {
            std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

/// The extension's running service worker in `context`.
async fn worker(browser: &Browser, context: &str) -> Result<Option<TargetInfo>> {
    let origin = catalog::origin(ID);
    Ok(browser.targets().await?.into_iter().find(|target| {
        Kind::of(target) == Some(Kind::Worker)
            && target.url.starts_with(&origin)
            && target.browser_context_id.as_deref() == Some(context)
    }))
}

/// Starts the stopped service worker through `via`'s session and waits for it in `context`.
async fn wake(browser: &Browser, via: &Page, context: &str) -> Result<Option<TargetInfo>> {
    let session = via.session()?;
    let cdp = &browser.cdp;
    cdp.send("ServiceWorker.enable", json!({}), Some(&session)).await?;
    let started = cdp.send("ServiceWorker.startWorker", json!({"scopeURL": catalog::origin(ID)}), Some(&session)).await;
    cdp.send("ServiceWorker.disable", json!({}), Some(&session)).await.ok();
    started.context("starting hba's extension")?;
    let deadline = Instant::now() + WORKER_WAIT;
    loop {
        if let Some(worker) = worker(browser, context).await? {
            return Ok(Some(worker));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(POLL).await;
    }
}
