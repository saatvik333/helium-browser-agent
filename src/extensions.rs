//! Extension commands: list, load, reload and remove; evaluate in the service worker; console
//! logs and Helium's error records; popup and options pages; `chrome.storage`; the worker's
//! lifecycle; and content-script worlds for `hba js --world`.
//!
//! Unpacked extensions go through the protocol's `Extensions` domain, which installs into Helium's
//! default (last-used) profile until Helium restarts. Listing reads the session profile's
//! preferences; workers and extension pages are debugged through their own targets.

mod catalog;
mod debugger;
mod manager;

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::browser::{Browser, TargetEvent};
use crate::cli::{ExtOp, ExtStorageArea, LoadState, StorageOp, SwAction};
use crate::console::Entry;
use crate::daemon::Cx;
use crate::output::{render_value, short_url, truncate};
use crate::page::{ExecutionContext, Page};
use crate::profiles::Profile;
use crate::undo::Undo;

use catalog::{Background, Extension, Location};
use debugger::{Debugger, Kind, running};
use manager::Manager;

pub use debugger::State;

/// Character budget for evaluation results, as `hba js` defaults to.
const RESULT_MAX: usize = 16_000;

/// Longest error message shown per line by `hba ext errors`.
const ERROR_TEXT_MAX: usize = 500;

/// Longest wait for an extension's content-script world after enabling `Runtime` on the page.
const WORLD_WAIT: Duration = Duration::from_secs(1);

/// Interval between checks for that world.
const WORLD_POLL: Duration = Duration::from_millis(25);

/// `hba ext …`: lists the session profile's extensions, or runs one extension operation.
pub async fn ext(cx: &mut Cx<'_>, op: Option<ExtOp>) -> Result<()> {
    let scope = Scope::new(cx).await?;
    let Some(op) = op else {
        return list(cx, &scope).await;
    };
    match op {
        ExtOp::Load { dir } => load(cx, &scope, &dir).await,
        ExtOp::Reload { ext } => {
            let ext = scope.find(cx, &ext).await?;
            reload(cx, &scope, &ext).await
        }
        ExtOp::Rm { ext } => {
            let ext = scope.find(cx, &ext).await?;
            remove(cx, &scope, &ext).await
        }
        ExtOp::Eval { ext, code } => {
            let ext = scope.find(cx, &ext).await?;
            let session = scope.debugger().worker(&ext).await?;
            let cdp = &scope.browser.cdp;
            let value = crate::page::evaluate_on(cdp, &session, None, &code, false, cx.timeout).await;
            // The worker session outlives the command, so its result objects are released here.
            cdp.send("Runtime.releaseObjectGroup", json!({"objectGroup": "hba"}), Some(&session)).await.ok();
            cx.say(render_value(&value?, RESULT_MAX));
            Ok(())
        }
        ExtOp::Logs { ext, last, clear } => {
            let ext = scope.find(cx, &ext).await?;
            logs(cx, &scope, &ext, last, clear).await
        }
        ExtOp::Errors { ext, clear } => {
            let ext = scope.find(cx, &ext).await?;
            errors(cx, &scope, &ext, clear).await
        }
        ExtOp::Popup { ext } => {
            let ext = scope.find(cx, &ext).await?;
            popup(cx, &scope, &ext).await
        }
        ExtOp::Open { ext, page } => {
            let ext = scope.find(cx, &ext).await?;
            let page = match page {
                Some(page) => page,
                None => ext.manifest.options.clone().with_context(|| {
                    format!("{} has no options page; name a page: `hba ext open {:?} PAGE`", ext.name(), ext.id)
                })?,
            };
            open_page(cx, &scope, &ext, &page).await
        }
        ExtOp::Storage { ext, area, op } => {
            let ext = scope.find(cx, &ext).await?;
            storage(cx, &scope, &ext, area, op).await
        }
        ExtOp::Sw { ext, action } => {
            let ext = scope.find(cx, &ext).await?;
            service_worker(cx, &scope, &ext, action).await
        }
    }
}

/// The session's profile in the connected browser.
struct Scope {
    browser: Arc<Browser>,
    profile: Profile,
    /// The profile's browser context, when hba has seen one of its tabs.
    context: Option<String>,
    /// Helium's default (last-used) profile, where the `Extensions` domain installs.
    default: Profile,
}

impl Scope {
    async fn new(cx: &Cx<'_>) -> Result<Self> {
        let browser = cx.browser().await?;
        let profile = cx.profile(&browser)?;
        let profiles = browser.profiles()?;
        let default_dir = profiles.last_used.clone().unwrap_or_else(|| "Default".to_owned());
        let default =
            profiles.by_dir(&default_dir).cloned().unwrap_or(Profile { name: default_dir.clone(), dir: default_dir });
        let context = browser.contexts().into_iter().find(|(_, dir)| *dir == profile.dir).map(|(context, _)| context);
        Ok(Self { browser, profile, context, default })
    }

    fn debugger(&self) -> Debugger<'_> {
        Debugger {
            browser: &self.browser,
            profile: &self.profile,
            context: self.context.as_deref(),
            default_profile: self.profile == self.default,
        }
    }

    /// The profile's extensions, sorted by name.
    async fn extensions(&self) -> Result<Vec<Extension>> {
        // Preferences files run to megabytes; reading and parsing them stays off the runtime thread.
        let profile_dir = self.browser.config().user_data_dir.join(&self.profile.dir);
        let mut list = tokio::task::spawn_blocking(move || catalog::installed(&profile_dir)).await??;
        // Without the Extensions domain the preferences still list everything, if seconds late.
        if self.profile == self.default
            && let Ok(unpacked) = loaded(&self.browser).await
        {
            catalog::merge_loaded(&mut list, unpacked);
        }
        Ok(list)
    }

    /// The extension `query` names: an id, the directory it was loaded from, or (part of) its name.
    async fn find(&self, cx: &Cx<'_>, query: &str) -> Result<Extension> {
        let looks_like_path = query.contains('/') || query.starts_with('.');
        let dir = looks_like_path.then(|| cx.path(Path::new(query)));
        let list = self.extensions().await?;
        catalog::resolve(&list, query, dir.as_deref()).cloned()
    }
}

/// Unpacked extensions of the default profile, as the browser reports them.
async fn loaded(browser: &Browser) -> Result<Vec<catalog::Loaded>> {
    #[derive(Deserialize)]
    struct List {
        extensions: Vec<catalog::Loaded>,
    }
    Ok(browser.cdp.call::<List>("Extensions.getExtensions", json!({}), None).await?.extensions)
}

async fn list(cx: &mut Cx<'_>, scope: &Scope) -> Result<()> {
    let debugger = scope.debugger();
    let (extensions, targets) = tokio::join!(scope.extensions(), scope.browser.targets());
    let (extensions, targets) = (extensions?, targets?);
    if extensions.is_empty() {
        cx.say(format!("no extensions in profile {}; `hba ext load DIR` installs an unpacked one", scope.profile.name));
        return Ok(());
    }
    let mut rows = Vec::with_capacity(extensions.len());
    for ext in &extensions {
        let own = debugger.select(&targets, &ext.id);
        let background = match ext.manifest.background {
            Background::Worker => Some(("service worker", running(&own, Kind::Worker).is_some())),
            Background::Page => Some(("background page", running(&own, Kind::BackgroundPage).is_some())),
            Background::None => None,
        };
        rows.push((ext, background.filter(|_| ext.enabled), debugger.capturing(&ext.id)));
    }
    if cx.json {
        let items: Vec<Value> = rows
            .iter()
            .map(|(ext, background, logs)| {
                json!({
                    "id": ext.id, "name": ext.name(), "version": ext.manifest.version, "enabled": ext.enabled,
                    "location": ext.location.label(), "path": ext.dir,
                    "background": background.map(|(kind, _)| kind), "running": background.is_some_and(|(_, up)| up),
                    "logs": logs,
                })
            })
            .collect();
        cx.say(Value::from(items).to_string());
        return Ok(());
    }
    for (ext, background, logs) in rows {
        let mut flags = vec![ext.location.label().to_owned()];
        if !ext.enabled {
            flags.push("disabled".to_owned());
        }
        if let Some((kind, up)) = background {
            flags.push(format!("{kind} {}", if up { "running" } else { "stopped" }));
        }
        if logs {
            flags.push("logs captured".to_owned());
        }
        let path =
            ext.dir.as_deref().filter(|_| ext.location == Location::Unpacked).map(|dir| format!(" {}", tilde(dir)));
        cx.say(format!(
            "{} {} {} [{}]{}",
            ext.id,
            ext.name(),
            ext.manifest.version,
            flags.join(", "),
            path.unwrap_or_default()
        ));
    }
    Ok(())
}

async fn load(cx: &mut Cx<'_>, scope: &Scope, dir: &Path) -> Result<()> {
    #[derive(Deserialize)]
    struct Installed {
        id: String,
    }
    let requested = cx.path(dir);
    let dir = std::fs::canonicalize(&requested).with_context(|| format!("no directory {}", requested.display()))?;
    if !dir.join("manifest.json").is_file() {
        bail!("{} has no manifest.json", tilde(&dir));
    }
    let path = dir.to_str().context("the directory path is not valid UTF-8")?;
    let before = loaded(&scope.browser).await?;
    // Loading a directory again reloads its extension; its current worker must not be mistaken for the new one.
    let previous = before.iter().find(|known| known.path == dir).map(|known| known.id.clone());
    let old = match &previous {
        Some(id) => worker_targets(scope, id).await?,
        None => Vec::new(),
    };
    let installed = scope.browser.cdp.call::<Installed>("Extensions.loadUnpacked", json!({"path": path}), None).await;
    let id = match installed {
        Ok(installed) => installed.id,
        Err(error) => bail!("Helium did not load {}: {}", tilde(&dir), error.root_cause()),
    };
    // The session's profile may not be the one Helium installed into, so ask the browser.
    let mut now = Vec::new();
    catalog::merge_loaded(&mut now, loaded(&scope.browser).await?);
    let ext = now
        .into_iter()
        .find(|known| known.id == id)
        .with_context(|| format!("Helium loaded {id} but does not list it"))?;
    let replaced = before.iter().any(|known| known.id == id);
    cx.say(format!(
        "{} {} {} — {id} (until Helium restarts)",
        if replaced { "reloaded" } else { "loaded" },
        ext.name(),
        ext.manifest.version
    ));
    if scope.profile != scope.default {
        cx.say(format!(
            "note: Helium installs unpacked extensions in its default profile {:?}; this session uses {:?}",
            scope.default.name, scope.profile.name
        ));
    }
    if replaced {
        follow_reload(cx, scope, &ext, &old).await;
    }
    Ok(())
}

/// Ids of the extension's worker targets, to tell its next worker from them.
async fn worker_targets(scope: &Scope, id: &str) -> Result<Vec<String>> {
    let targets = scope.debugger().targets(id).await?;
    Ok(targets
        .into_iter()
        .filter(|target| Kind::of(target) == Some(Kind::Worker))
        .map(|target| target.target_id)
        .collect())
}

/// After a reload: drops sessions of the old worker and, when the console is captured, attaches
/// to the new worker.
async fn follow_reload(cx: &mut Cx<'_>, scope: &Scope, ext: &Extension, old: &[String]) {
    let debugger = scope.debugger();
    debugger.release(&ext.id).await;
    if !debugger.capturing(&ext.id) {
        return;
    }
    if ext.manifest.background == Background::Worker
        && let Err(error) = debugger.wait_worker(ext, old).await
    {
        cx.say(format!("logs: {error:#}"));
        return;
    }
    match debugger.capture(ext).await {
        Ok(_) => cx.say("logs: still capturing (attached to the reloaded extension)"),
        Err(error) => cx.say(format!("logs: {error:#}")),
    }
}

async fn reload(cx: &mut Cx<'_>, scope: &Scope, ext: &Extension) -> Result<()> {
    let debugger = scope.debugger();
    let old = worker_targets(scope, &ext.id).await?;
    let manager = Manager::open(&debugger).await?;
    let outcome = manager.reload(&ext.id).await;
    manager.close().await;
    if let Some(failure) = outcome? {
        debugger.release(&ext.id).await;
        bail!("{} did not reload: {}\nit stays disabled until a reload succeeds", ext.name(), failure.describe());
    }
    let fresh = scope.find(cx, &ext.id).await.unwrap_or_else(|_| ext.clone());
    let version = if fresh.manifest.version == ext.manifest.version {
        fresh.manifest.version.clone()
    } else {
        format!("{} (was {})", fresh.manifest.version, ext.manifest.version)
    };
    cx.say(format!("reloaded {} {version}", fresh.name()));
    follow_reload(cx, scope, &fresh, &old).await;
    Ok(())
}

async fn remove(cx: &mut Cx<'_>, scope: &Scope, ext: &Extension) -> Result<()> {
    if ext.location != Location::Unpacked {
        bail!(
            "{} is a {} extension; hba removes only unpacked ones (remove it in chrome://extensions)",
            ext.name(),
            ext.location.label()
        );
    }
    scope
        .browser
        .cdp
        .send("Extensions.uninstall", json!({"id": ext.id}), None)
        .await
        .with_context(|| format!("removing {}", ext.name()))?;
    scope.debugger().forget(&ext.id).await;
    cx.say(format!("removed {} — {}", ext.name(), ext.id));
    Ok(())
}

async fn logs(cx: &mut Cx<'_>, scope: &Scope, ext: &Extension, last: usize, clear: bool) -> Result<()> {
    let debugger = scope.debugger();
    let capture = debugger.capture(ext).await?;
    if clear {
        let count = debugger.clear(&ext.id);
        let plural = if count == 1 { "" } else { "s" };
        cx.say(format!("cleared {count} message{plural} of {}", ext.name()));
        return Ok(());
    }
    let (earlier, entries) = debugger.entries(&ext.id, last);
    if cx.json {
        let items: Vec<Value> = entries.iter().map(Entry::json).collect();
        cx.say(Value::from(items).to_string());
        return Ok(());
    }
    if capture.started {
        let worker = ext.manifest.background == Background::Worker;
        let mut sources = Vec::new();
        match (capture.worker, worker, capture.woke) {
            (false, ..) => {}
            (true, false, _) => sources.push("its background page".to_owned()),
            (true, true, false) => sources.push("its service worker".to_owned()),
            (true, true, true) => sources.push("its service worker (started by hba)".to_owned()),
        }
        if capture.pages > 0 {
            let plural = if capture.pages == 1 { "" } else { "s" };
            sources.push(format!("{} page{plural}", capture.pages));
        }
        let from = if sources.is_empty() { "its pages as they open".to_owned() } else { sources.join(" and ") };
        let kept = if capture.worker && worker { "; hba stays attached, which keeps the worker running" } else { "" };
        cx.say(format!("capturing the console of {} from {from}{kept}", ext.name()));
    } else if entries.is_empty() {
        cx.say(format!("no messages from {} yet", ext.name()));
    }
    if earlier > 0 {
        let plural = if earlier == 1 { "" } else { "s" };
        cx.say(format!("… {earlier} earlier message{plural} (raise -n)"));
    }
    for entry in &entries {
        cx.say(entry.line());
    }
    Ok(())
}

async fn errors(cx: &mut Cx<'_>, scope: &Scope, ext: &Extension, clear: bool) -> Result<()> {
    let manager = Manager::open(&scope.debugger()).await?;
    let report = manager.errors(&ext.id, clear).await;
    manager.close().await;
    let report = report?;
    if cx.json {
        cx.say(serde_json::to_string(&report)?);
        return Ok(());
    }
    let count = report.manifest.len() + report.runtime.len();
    let plural = if count == 1 { "" } else { "s" };
    cx.say(match (count, clear) {
        (0, _) => format!("no errors recorded for {}", ext.name()),
        (_, true) => format!("cleared {count} error{plural} of {}", ext.name()),
        (_, false) => format!("{count} error{plural} recorded for {}", ext.name()),
    });
    if report.enabled_developer_mode {
        cx.say(
            "turned on developer mode: Helium records runtime errors only in developer mode, so earlier ones were not kept; reproduce the problem and run this again",
        );
    }
    if clear {
        return Ok(());
    }
    for warning in &report.manifest {
        cx.say(format!("manifest {}", warning.message));
    }
    for error in &report.runtime {
        cx.say(truncate(&error.summary(), ERROR_TEXT_MAX).replace('\n', "\n  "));
    }
    Ok(())
}

async fn popup(cx: &mut Cx<'_>, scope: &Scope, ext: &Extension) -> Result<()> {
    if let Some(popup) = &ext.manifest.popup {
        return open_page(cx, scope, ext, popup).await;
    }
    if !ext.manifest.action {
        bail!("{} has no toolbar action", ext.name());
    }
    let browser = &scope.browser;
    let page = cx.page().await?;
    let mut created = browser.subscribe();
    let watch = cx.watch(browser).await;
    let mark = page.mark();
    browser
        .cdp
        .send("Extensions.triggerAction", json!({"id": ext.id, "targetId": page.tab}), None)
        .await
        .with_context(|| format!("running {}'s action", ext.name()))?;
    let label = cx.tab_number(&page.tab);
    cx.say(format!("ran {}'s action on {label}", ext.name()));
    crate::actions::after(cx, browser, &page, &mark, watch, false).await?;
    let origin = ext.origin();
    let mut opened = 0;
    while let Ok(event) = created.try_recv() {
        if let TargetEvent::Created(info) = event
            && info.url.starts_with(&origin)
        {
            cx.say(format!("+ {} {}", info.kind, short_url(&info.url, 120)));
            opened += 1;
        }
    }
    if opened == 0 {
        cx.say(format!("it opened no page of its own; `hba ext logs {:?}` shows what it logged", ext.id));
    }
    let debugger = scope.debugger();
    if debugger.capturing(&ext.id) {
        debugger.capture(ext).await?;
    }
    Ok(())
}

/// Opens an extension page in a new session tab, which becomes current.
async fn open_page(cx: &mut Cx<'_>, scope: &Scope, ext: &Extension, page: &str) -> Result<()> {
    if !ext.enabled {
        bail!("{} is disabled", ext.name());
    }
    if let Some(dir) = &ext.dir
        && !page.contains("://")
    {
        let file = page.split(['?', '#']).next().unwrap_or(page).trim_start_matches("./").trim_start_matches('/');
        if !dir.join(file).is_file() {
            bail!("{} has no {file}", ext.name());
        }
    }
    let url = ext.url(page);
    let tab = cx.new_tab(false).await?;
    tab.goto(&url, LoadState::Load, cx.timeout).await?;
    let debugger = scope.debugger();
    if debugger.capturing(&ext.id) {
        debugger.capture(ext).await?;
    }
    let label = cx.tab_number(&tab.tab);
    let title = scope.browser.tab_info(&tab.tab).await?.map(|info| info.title).unwrap_or_default();
    let title =
        if title.is_empty() || url.ends_with(&title) { String::new() } else { format!(" {:?}", truncate(&title, 80)) };
    cx.say(format!("{label} {}{title}", short_url(&tab.url(), 120)));
    Ok(())
}

async fn storage(
    cx: &mut Cx<'_>,
    scope: &Scope,
    ext: &Extension,
    area: ExtStorageArea,
    op: Option<StorageOp>,
) -> Result<()> {
    #[derive(Deserialize)]
    struct Items {
        data: Map<String, Value>,
    }
    if area == ExtStorageArea::Managed && !matches!(op, None | Some(StorageOp::Get { .. })) {
        bail!("managed storage is read-only: policy sets it");
    }
    let area = match area {
        ExtStorageArea::Local => "local",
        ExtStorageArea::Sync => "sync",
        ExtStorageArea::Session => "session",
        ExtStorageArea::Managed => "managed",
    };
    let session = scope.debugger().storage_session(ext).await?;
    let cdp = &scope.browser.cdp;
    let params = |extra: Value| {
        let mut params = json!({"id": ext.id, "storageArea": area});
        if let (Some(params), Value::Object(extra)) = (params.as_object_mut(), extra) {
            params.extend(extra);
        }
        params
    };
    let name = ext.name();
    match op {
        None => {
            let items: Items = cdp.call("Extensions.getStorageItems", params(json!({})), Some(&session)).await?;
            if cx.json {
                cx.say(Value::Object(items.data).to_string());
            } else if items.data.is_empty() {
                cx.say(format!("{area} storage of {name} is empty"));
            } else {
                for (key, value) in &items.data {
                    cx.say(format!("{key} = {}", truncate(&value.to_string(), RESULT_MAX)));
                }
            }
        }
        Some(StorageOp::Get { key }) => {
            let items: Items =
                cdp.call("Extensions.getStorageItems", params(json!({"keys": [key]})), Some(&session)).await?;
            let value = items.data.get(&key).with_context(|| format!("no {key:?} in {area} storage of {name}"))?;
            cx.say(if cx.json { value.to_string() } else { render_value(value, RESULT_MAX) });
        }
        Some(StorageOp::Set { key, value }) => {
            let value: Value = serde_json::from_str(&value).unwrap_or(Value::String(value));
            let mut values = Map::new();
            values.insert(key.clone(), value.clone());
            cdp.send("Extensions.setStorageItems", params(json!({"values": values})), Some(&session)).await?;
            cx.say(format!("set {area} {key} = {}", truncate(&value.to_string(), RESULT_MAX)));
        }
        Some(StorageOp::Rm { key }) => {
            cdp.send("Extensions.removeStorageItems", params(json!({"keys": [key]})), Some(&session)).await?;
            cx.say(format!("removed {area} {key}"));
        }
        Some(StorageOp::Clear) => {
            cdp.send("Extensions.clearStorageItems", params(json!({})), Some(&session)).await?;
            cx.say(format!("cleared {area} storage of {name}"));
        }
    }
    Ok(())
}

async fn service_worker(cx: &mut Cx<'_>, scope: &Scope, ext: &Extension, action: SwAction) -> Result<()> {
    let name = ext.name();
    let debugger = scope.debugger();
    match ext.manifest.background {
        Background::Worker => {}
        Background::None => bail!("{name} has no service worker"),
        Background::Page if action == SwAction::Status => {
            let up = running(&debugger.targets(&ext.id).await?, Kind::BackgroundPage).is_some();
            cx.say(format!(
                "{name} has a background page, not a service worker; it is {}",
                if up { "running" } else { "not running" }
            ));
            return Ok(());
        }
        Background::Page => {
            bail!("{name} has a background page, not a service worker; `hba ext reload {:?}` restarts it", ext.id)
        }
    }
    let first = match action {
        SwAction::Status => None,
        SwAction::Start => Some(if debugger.start_worker(ext).await? { "started" } else { "already running" }),
        SwAction::Stop => Some(if debugger.stop_worker(ext).await? { "stopped" } else { "already stopped" }),
    };
    let up = running(&debugger.targets(&ext.id).await?, Kind::Worker).is_some();
    let attached = debugger.worker_attached(&ext.id);
    let logs = debugger.capturing(&ext.id);
    if cx.json {
        cx.say(json!({"running": up, "attached": attached, "logs": logs}).to_string());
        return Ok(());
    }
    let mut line = format!("{name}'s service worker {}", first.unwrap_or(if up { "is running" } else { "is stopped" }));
    match (up, attached) {
        (true, true) => line.push_str("; hba is attached, which keeps it running"),
        (true, false) => line.push_str("; it stops after about 30 s without events"),
        (false, true) => line
            .push_str("; it starts on its next event and hba stays attached for its logs, which then keeps it running"),
        (false, false) => {
            let _ = write!(line, "; it starts on its next event or with `hba ext sw {:?} start`", ext.id);
        }
    }
    if logs && attached {
        line.push_str("; logs captured");
    }
    cx.say(line);
    Ok(())
}

/// Evaluates `code` in the content-script world of `extension` (id or part of its name) in the
/// page's main frame. `Runtime` reports the worlds; it is enabled on the page's session for this
/// call only, which a website can notice, so the output says so there.
pub async fn world_eval(cx: &mut Cx<'_>, page: &Arc<Page>, extension: &str, code: &str) -> Result<Value> {
    let outcome = in_world(page, extension, code).await;
    if !page.is_local() && !cx.json {
        cx.say("Runtime was enabled on this website for the call, which anti-bot scripts can detect");
    }
    outcome
}

/// [`world_eval`] without the message: `Runtime` is on from before the extension's world is
/// looked for until after `code` ran, and off again however the call ends, cancelled included.
async fn in_world(page: &Arc<Page>, extension: &str, code: &str) -> Result<Value> {
    let session = page.session()?;
    let frame = page.main_frame()?;
    // Contexts of this session are recorded only while this call keeps `Runtime` on.
    page.forget_contexts(&session);
    // Armed before `Runtime.enable`, so a client that hangs up still gets it turned off.
    let disable = Undo::new(disable_runtime(Arc::clone(page), Arc::clone(&session)));
    let outcome = page
        .guarded(async {
            page.cdp.send("Runtime.enable", json!({}), Some(&session)).await?;
            let context = world_context(page, &session, &frame, extension).await?;
            crate::page::evaluate_on(&page.cdp, &session, Some(context), code, false, crate::cdp::CALL_TIMEOUT).await
        })
        .await;
    // A page showing a dialog answers nothing until it closes; dropping the guard leaves the
    // undo running in the background.
    if page.dialog().is_some() {
        drop(disable);
    } else {
        disable.finish().await;
    }
    outcome
}

/// Turns `Runtime` off after `js --world` and forgets the contexts it reported.
async fn disable_runtime(page: Arc<Page>, session: Arc<str>) {
    page.cdp.send("Runtime.disable", json!({}), Some(&session)).await.ok();
    page.forget_contexts(&session);
}

/// Waits briefly for the extension's world among the contexts `Runtime` reports.
async fn world_context(page: &Page, session: &str, frame: &str, query: &str) -> Result<i64> {
    let deadline = Instant::now() + WORLD_WAIT;
    loop {
        let contexts = page.execution_contexts();
        let worlds = extension_worlds(&contexts, session, frame);
        if let Some(id) = pick_world(&worlds, query)? {
            return Ok(id);
        }
        if Instant::now() >= deadline {
            let mut names: Vec<&str> = worlds.iter().map(|world| world.name.as_str()).collect();
            names.dedup();
            let here = if names.is_empty() { "none".to_owned() } else { names.join(", ") };
            bail!("no content script of {query:?} runs in this page's main frame; extension worlds here: {here}");
        }
        tokio::time::sleep(WORLD_POLL).await;
    }
}

/// Extension content-script worlds of the main frame, newest last.
fn extension_worlds<'a>(contexts: &'a [ExecutionContext], session: &str, frame: &str) -> Vec<&'a ExecutionContext> {
    contexts
        .iter()
        .filter(|context| {
            &*context.session == session
                && context.frame.as_deref() == Some(frame)
                && context.kind == "isolated"
                && context.origin.starts_with("chrome-extension://")
        })
        .collect()
}

/// The world of the extension `query` names: its id, its exact name, or a unique part of it.
fn pick_world(worlds: &[&ExecutionContext], query: &str) -> Result<Option<i64>> {
    let lower = query.to_lowercase();
    let tiers: [&dyn Fn(&ExecutionContext) -> bool; 3] = [
        &|world| world.origin.strip_prefix("chrome-extension://") == Some(query),
        &|world| world.name.to_lowercase() == lower,
        &|world| world.name.to_lowercase().contains(&lower),
    ];
    for matches in tiers {
        // A world reported again after a reload replaces the older entry.
        let mut found: Vec<&ExecutionContext> = Vec::new();
        for world in worlds.iter().copied().filter(|world| matches(world)) {
            found.retain(|seen| seen.origin != world.origin);
            found.push(world);
        }
        match found.as_slice() {
            [one] => return Ok(Some(one.id)),
            [] => {}
            many => bail!(
                "{query:?} matches several extensions here: {}; use an id",
                many.iter().map(|world| format!("{} ({})", world.name, world.origin)).collect::<Vec<_>>().join(", ")
            ),
        }
    }
    Ok(None)
}

/// `path` with the home directory shortened to `~`.
fn tilde(path: &Path) -> String {
    shorten(path, &crate::config::home())
}

fn shorten(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "ilidgonjdoemebfgciljbbklchfokkln";
    const OTHER: &str = "blockjmkbacgjkknlgpkjjiijinjdanf";

    fn world(id: i64, origin: &str, name: &str, kind: &str, frame: &str) -> ExecutionContext {
        ExecutionContext {
            id,
            session: Arc::from("S"),
            origin: origin.to_owned(),
            name: name.to_owned(),
            kind: kind.to_owned(),
            frame: Some(frame.to_owned()),
        }
    }

    #[tokio::test]
    async fn a_cancelled_world_call_turns_runtime_off() {
        use crate::page::tests::{attached_page, eventually, sessions_of};
        let (page, _cdp, received) = attached_page().await;
        // No extension world shows up, so the call is still looking for it when it is dropped,
        // as the daemon drops a command whose client hung up.
        let call = in_world(&page, ID, "1");
        assert!(tokio::time::timeout(Duration::from_millis(100), call).await.is_err(), "the call ended early");
        assert_eq!(sessions_of(&received, "Runtime.enable"), ["page"]);
        assert!(
            eventually(|| sessions_of(&received, "Runtime.disable") == ["page"]).await,
            "Runtime stayed on: {:?}",
            sessions_of(&received, "Runtime.disable")
        );
    }

    #[test]
    fn content_script_worlds() {
        let contexts = [
            world(1, "http://127.0.0.1:8765", "", "default", "F"),
            world(2, &format!("chrome-extension://{ID}"), "hba test extension", "isolated", "F"),
            world(3, &format!("chrome-extension://{OTHER}"), "uBlock Origin", "isolated", "F"),
            world(4, &format!("chrome-extension://{ID}"), "hba test extension", "isolated", "CHILD"),
            world(5, "", "hba", "isolated", "F"),
            world(6, &format!("chrome-extension://{ID}"), "hba test extension", "isolated", "F"),
        ];
        let worlds = extension_worlds(&contexts, "S", "F");
        assert_eq!(worlds.iter().map(|w| w.id).collect::<Vec<_>>(), [2, 3, 6], "main-frame extension worlds only");
        assert_eq!(pick_world(&worlds, ID).ok().flatten(), Some(6), "the newest world of an extension wins");
        assert_eq!(pick_world(&worlds, "ublock").ok().flatten(), Some(3));
        assert_eq!(pick_world(&worlds, "HBA TEST EXTENSION").ok().flatten(), Some(6));
        assert_eq!(pick_world(&worlds, "nothing").ok().flatten(), None);
        let ambiguous = pick_world(&worlds, "o").expect_err("both names contain o").to_string();
        assert!(ambiguous.contains("uBlock Origin") && ambiguous.contains("hba test extension"), "{ambiguous}");
    }

    #[test]
    fn home_is_shortened() {
        let home = Path::new("/home/someone");
        assert_eq!(shorten(Path::new("/home/someone/codes/ext"), home), "~/codes/ext");
        assert_eq!(shorten(Path::new("/elsewhere/ext"), home), "/elsewhere/ext");
        assert_eq!(shorten(Path::new("/home/someone2/ext"), home), "/home/someone2/ext", "whole components only");
    }
}
