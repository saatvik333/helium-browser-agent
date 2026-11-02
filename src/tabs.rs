//! Tabs, windows, profiles, session status and cleanup.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::browser::{Browser, TargetInfo};
use crate::cli::{CloseArgs, LoadState, TabOp, TabsArgs, WindowArgs, WindowState};
use crate::daemon::Cx;
use crate::output::{short_url, truncate};
use crate::undo::Undo;

/// One tab as listed to the user (and as `--json`).
#[derive(Debug, Serialize)]
struct Row {
    tab: String,
    id: String,
    title: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window: Option<i64>,
    shown: bool,
    pinned: bool,
    grouped: bool,
    /// Session holding the tab, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    current: bool,
    adopted: bool,
}

impl Row {
    fn new(cx: &Cx<'_>, info: &TargetInfo) -> Self {
        let strip = info.embedder_data.clone().unwrap_or_default();
        let (session, adopted) = {
            let store = cx.hub.store.lock().expect("store lock");
            let holder = store.holder(&info.target_id).map(str::to_owned);
            let adopted = holder
                .as_deref()
                .and_then(|name| store.sessions.get(name))
                .is_some_and(|s| s.tabs.iter().any(|t| t.id == info.target_id && t.adopted));
            (holder, adopted)
        };
        let current = session.as_deref() == Some(cx.session.as_str())
            && cx.session_data().current.as_deref() == Some(info.target_id.as_str());
        Self {
            tab: cx.tab_number(&info.target_id),
            id: info.target_id.clone(),
            title: info.title.clone(),
            url: info.url.clone(),
            profile: None,
            window: None,
            shown: strip.active,
            pinned: strip.pinned,
            grouped: strip.group.is_some(),
            session,
            current,
            adopted,
        }
    }

    /// `t5 [current shown] Title — host/path`; the session marker only for other sessions' tabs.
    fn line(&self, own_session: &str) -> String {
        let mut flags: Vec<String> = Vec::new();
        for (on, name) in [
            (self.current, "current"),
            (self.shown, "shown"),
            (self.pinned, "pinned"),
            (self.grouped, "grouped"),
            (self.adopted, "adopted"),
        ] {
            if on {
                flags.push(name.to_owned());
            }
        }
        if let Some(session) = self.session.as_deref().filter(|s| *s != own_session) {
            flags.push(format!("hba:{session}"));
        }
        let mut line = self.tab.clone();
        if !flags.is_empty() {
            let _ = write!(line, " [{}]", flags.join(" "));
        }
        if !self.title.is_empty() && self.title != self.url {
            let _ = write!(line, " {}", truncate(&self.title, 70));
        }
        let _ = write!(line, " — {}", short_url(&self.url, 80));
        line
    }
}

fn print_json(cx: &mut Cx<'_>, value: &impl Serialize) -> Result<()> {
    cx.say(serde_json::to_string(value)?);
    Ok(())
}

pub async fn list(cx: &mut Cx<'_>, args: TabsArgs) -> Result<()> {
    let browser = cx.browser().await?;
    cx.live_tabs(&browser).await?;
    let tabs = browser.tabs().await?;
    if !args.all {
        let session = cx.session_data();
        let rows: Vec<Row> = tabs.iter().filter(|t| session.owns(&t.target_id)).map(|t| Row::new(cx, t)).collect();
        if cx.json {
            return print_json(cx, &rows);
        }
        if rows.is_empty() {
            cx.say(format!(
                "session {:?} has no tabs; `hba open URL`, or `hba tabs --all` to see every tab",
                cx.session
            ));
        }
        for row in &rows {
            let line = row.line(&cx.session);
            cx.say(line);
        }
        return Ok(());
    }
    let windows = join_all(tabs.iter().map(|tab| window_of(&browser, &tab.target_id))).await;
    let profiles = browser.profiles().ok();
    let mut rows: Vec<Row> = Vec::with_capacity(tabs.len());
    for (info, window) in tabs.iter().zip(windows) {
        let mut row = Row::new(cx, info);
        let context = info.browser_context_id.clone().unwrap_or_default();
        row.profile = Some(
            browser
                .profile_of_context(&context)
                .and_then(|dir| profiles.as_ref().and_then(|p| p.by_dir(&dir)).map(|p| p.name.clone()))
                .unwrap_or_else(|| format!("profile {}", &context[..context.len().min(6)])),
        );
        row.window = window;
        rows.push(row);
    }
    let order: BTreeMap<&str, i64> =
        tabs.iter().map(|t| (t.target_id.as_str(), t.embedder_data.as_ref().map_or(i64::MAX, |s| s.index))).collect();
    rows.sort_by(|a, b| {
        (&a.profile, a.window, order[a.id.as_str()]).cmp(&(&b.profile, b.window, order[b.id.as_str()]))
    });
    if cx.json {
        return print_json(cx, &rows);
    }
    let mut heading = None;
    for row in &rows {
        let group = (row.profile.clone().unwrap_or_default(), row.window.unwrap_or(-1));
        if heading.as_ref() != Some(&group) {
            cx.say(format!("{} · window {}", group.0, group.1));
            heading = Some(group);
        }
        let line = row.line(&cx.session);
        cx.say(format!("  {line}"));
    }
    Ok(())
}

async fn window_of(browser: &Browser, tab: &str) -> Option<i64> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Window {
        window_id: i64,
    }
    browser
        .cdp
        .call::<Window>("Browser.getWindowForTarget", json!({"targetId": tab}), None)
        .await
        .ok()
        .map(|w| w.window_id)
}

/// Prints one tab: a line, or with `--json` an object that also carries the page target id
/// (what other CDP tools attach to).
async fn show_tab(cx: &mut Cx<'_>, browser: &Browser, tab: &str, page_target: Option<String>) -> Result<()> {
    let info = browser.tab_info(tab).await?.context("the tab closed")?;
    let row = Row::new(cx, &info);
    if cx.json {
        let mut value = serde_json::to_value(&row)?;
        value["page"] = json!(page_target);
        return print_json(cx, &value);
    }
    let line = row.line(&cx.session);
    cx.say(line);
    Ok(())
}

pub async fn tab(cx: &mut Cx<'_>, op: TabOp) -> Result<()> {
    let browser = cx.browser().await?;
    match op {
        TabOp::New { url, front } => {
            let page = cx.new_tab(front).await?;
            if let Some(url) = url {
                let url = crate::nav::normalize_url(&url, &cx.cwd);
                page.goto(&url, LoadState::Dom, cx.timeout).await?;
            }
            show_tab(cx, &browser, &page.tab, page.page_target()).await
        }
        TabOp::Use { id } => {
            let tab = cx.resolve_any_tab(&browser, &id).await?;
            // Checked and claimed under one lock, so two sessions cannot both take the tab.
            let claimed = {
                let mut store = cx.hub.store.lock().expect("store lock");
                if let Some(holder) = store.holder(&tab).filter(|holder| *holder != cx.session) {
                    bail!(
                        "{id} is used by session {holder:?}; release it there first (`hba -s {holder} tab release {id}`)"
                    );
                }
                let session = store.session(&cx.session);
                let claimed = !session.owns(&tab);
                if claimed {
                    // Not opened by this session: the user's tab, released instead of closed.
                    session.add(&tab, true);
                    store.save()?;
                }
                claimed
            };
            // A failed or cancelled attach gives the tab back.
            let unclaim = Undo::new({
                let (hub, session, tab) = (Arc::clone(cx.hub), cx.session.clone(), tab.clone());
                async move {
                    if claimed {
                        let mut store = hub.store.lock().expect("store lock");
                        store.session(&session).remove(&tab);
                        store.save().ok();
                    }
                }
            });
            let page = match browser.attach(&tab).await {
                Ok(page) => page,
                Err(error) => {
                    unclaim.finish().await;
                    return Err(error);
                }
            };
            unclaim.disarm();
            cx.update_session(|session| session.current = Some(tab.clone()))?;
            show_tab(cx, &browser, &tab, page.page_target()).await
        }
        TabOp::Close { id } => {
            let tab = session_tab(cx, &browser, id.as_deref()).await?;
            let label = cx.tab_number(&tab);
            browser.close_target(&tab).await?;
            cx.update_session(|session| session.remove(&tab))?;
            cx.say(format!("closed {label}"));
            Ok(())
        }
        TabOp::Release { id } => {
            let tab = session_tab(cx, &browser, id.as_deref()).await?;
            let label = cx.tab_number(&tab);
            browser.detach(&tab).await;
            cx.update_session(|session| session.remove(&tab))?;
            cx.say(format!("released {label}; it stays open"));
            Ok(())
        }
        TabOp::Front { id } => {
            let tab = session_tab(cx, &browser, id.as_deref()).await?;
            browser.cdp.send("Target.activateTarget", json!({"targetId": tab}), None).await?;
            let label = cx.tab_number(&tab);
            cx.say(format!("showing {label}"));
            Ok(())
        }
    }
}

/// A tab of this session: the given reference or the current tab.
async fn session_tab(cx: &Cx<'_>, browser: &Browser, id: Option<&str>) -> Result<String> {
    match id {
        Some(id) => {
            let tab = cx.resolve_any_tab(browser, id).await?;
            if !cx.session_data().owns(&tab) {
                bail!("{id} is not in session {:?}", cx.session);
            }
            Ok(tab)
        }
        None => cx.current_tab(browser).await,
    }
}

pub async fn window(cx: &mut Cx<'_>, args: WindowArgs) -> Result<()> {
    let browser = cx.browser().await?;
    let tab = cx.current_tab(&browser).await?;
    let window = window_of(&browser, &tab).await.context("the tab has no window")?;
    let bounds = match (args.state, args.size) {
        (WindowState::Size, Some((width, height))) => {
            browser
                .cdp
                .send("Browser.setWindowBounds", json!({"windowId": window, "bounds": {"windowState": "normal"}}), None)
                .await?;
            json!({"width": width, "height": height})
        }
        (WindowState::Size, None) => bail!("give a size: `hba window size 1280x800`"),
        (state, _) => json!({"windowState": match state {
            WindowState::Maximized => "maximized",
            WindowState::Minimized => "minimized",
            WindowState::Fullscreen => "fullscreen",
            WindowState::Normal | WindowState::Size => "normal",
        }}),
    };
    browser.cdp.send("Browser.setWindowBounds", json!({"windowId": window, "bounds": bounds}), None).await?;
    let now: serde_json::Value = browser.cdp.call("Browser.getWindowBounds", json!({"windowId": window}), None).await?;
    let b = &now["bounds"];
    cx.say(format!(
        "window {window} {} {}x{}",
        b["windowState"].as_str().unwrap_or("normal"),
        b["width"].as_i64().unwrap_or(0),
        b["height"].as_i64().unwrap_or(0)
    ));
    Ok(())
}

pub async fn profiles(cx: &mut Cx<'_>) -> Result<()> {
    #[derive(Serialize)]
    struct ProfileRow<'a> {
        name: &'a str,
        dir: &'a str,
        /// Configured aliases that pick this profile.
        aliases: Vec<&'a str>,
        open: bool,
        session: bool,
        last_used: bool,
    }
    let config = cx.config()?;
    let profiles = crate::profiles::Profiles::load(&config.user_data_dir)?;
    let open: Vec<String> = match cx.hub.connected().await {
        Some(browser) => browser.contexts().into_values().collect(),
        None => profiles.open.clone(),
    };
    let session_profile = cx.session_data().profile;
    let rows: Vec<ProfileRow<'_>> = profiles
        .list
        .iter()
        .map(|p| ProfileRow {
            name: &p.name,
            dir: &p.dir,
            aliases: config
                .aliases
                .keys()
                .filter(|alias| profiles.resolve(alias, &config.aliases).is_ok_and(|hit| hit.dir == p.dir))
                .map(String::as_str)
                .collect(),
            open: open.contains(&p.dir),
            session: session_profile.as_deref() == Some(p.dir.as_str()),
            last_used: profiles.last_used.as_deref() == Some(p.dir.as_str()),
        })
        .collect();
    if cx.json {
        let text = serde_json::to_string(&rows)?;
        cx.say(text);
        return Ok(());
    }
    let lines: Vec<String> = rows
        .iter()
        .map(|row| {
            let flags: Vec<&str> = [(row.open, "open"), (row.session, "session"), (row.last_used, "last used")]
                .into_iter()
                .filter_map(|(on, name)| on.then_some(name))
                .collect();
            let flags = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join(", ")) };
            let aliases = if row.aliases.is_empty() { String::new() } else { format!(" -p {}", row.aliases.join("|")) };
            format!("{} ({}){flags}{aliases}", row.name, row.dir)
        })
        .collect();
    for line in lines {
        cx.say(line);
    }
    Ok(())
}

/// The browser if it is running; never launches Helium (for status and cleanup).
async fn running_browser(cx: &Cx<'_>) -> Result<Option<Arc<Browser>>> {
    if let Some(browser) = cx.hub.connected().await {
        return Ok(Some(browser));
    }
    match crate::endpoint::discover(&*cx.config()?) {
        Ok(_) => cx.browser().await.map(Some),
        Err(_) => Ok(None),
    }
}

pub async fn status(cx: &mut Cx<'_>) -> Result<()> {
    let uptime = cx.hub.started.elapsed().as_secs();
    let Some(browser) = running_browser(cx).await? else {
        if cx.json {
            return print_json(cx, &json!({"session": cx.session, "browser": null}));
        }
        cx.say(format!("session {} · Helium is not running (`hba open URL` starts it)", cx.session));
        return Ok(());
    };
    let profile = cx.chosen_profile(&browser).ok();
    cx.live_tabs(&browser).await?;
    let session = cx.session_data();
    let mut rows = Vec::new();
    for tab in &session.tabs {
        if let Some(info) = browser.tab_info(&tab.id).await? {
            rows.push(Row::new(cx, &info));
        }
    }
    let dialog =
        session.current.as_deref().and_then(|current| browser.attached(current)).and_then(|page| page.dialog());
    if cx.json {
        return print_json(
            cx,
            &json!({
                "session": cx.session,
                "browser": browser.product,
                "pid": browser.endpoint.pid,
                "profile": profile.as_ref().map(|p| &p.name),
                "tabs": rows,
                "dialog": dialog.as_ref().map(|d| json!({"type": d.kind, "message": d.message})),
            }),
        );
    }
    let pid = browser.endpoint.pid.map(|pid| format!(" pid {pid}")).unwrap_or_default();
    let profile = profile.map(|p| format!(" · profile {}", p.name)).unwrap_or_default();
    cx.say(format!("session {}{profile} · {}{pid} · daemon up {}m", cx.session, browser.product, uptime / 60));
    if rows.is_empty() {
        cx.say("no tabs; `hba open URL`");
    }
    for row in &rows {
        let line = row.line(&cx.session);
        cx.say(line);
    }
    if let Some(dialog) = dialog {
        cx.say(format!("! {} dialog open: {:?}", dialog.kind, dialog.message));
    }
    Ok(())
}

pub async fn close(cx: &mut Cx<'_>, args: CloseArgs) -> Result<()> {
    let names: Vec<String> = if args.all {
        cx.hub.store.lock().expect("store lock").sessions.keys().cloned().collect()
    } else {
        vec![cx.session.clone()]
    };
    let tabs: Vec<(String, crate::session::Tab)> = {
        let store = cx.hub.store.lock().expect("store lock");
        names
            .iter()
            .filter_map(|name| store.sessions.get(name).map(|s| (name, s)))
            .flat_map(|(name, session)| session.tabs.iter().map(move |tab| (name.clone(), tab.clone())))
            .collect()
    };
    if tabs.is_empty() {
        cx.say("nothing to close");
        return Ok(());
    }
    let Some(browser) = running_browser(cx).await? else {
        bail!("Helium is not running; session state kept for a later `hba close`");
    };
    let (mut closed, mut released) = (0, 0);
    for (name, tab) in &tabs {
        if tab.adopted {
            browser.detach(&tab.id).await;
            released += 1;
        } else if browser.tab_info(&tab.id).await?.is_some() {
            browser.close_target(&tab.id).await?;
            closed += 1;
        }
        let mut store = cx.hub.store.lock().expect("store lock");
        store.session(name).remove(&tab.id);
        store.save()?;
    }
    cx.say(format!("closed {}, released {released}", crate::output::count(closed, "tab")));
    Ok(())
}
