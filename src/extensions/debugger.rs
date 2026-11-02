//! Debugger sessions on extension targets (service workers, background pages, extension pages),
//! the console captured through them, evaluation, and short-lived helper pages.
//!
//! A session stays attached once made. An attached debugger keeps a service worker running, as
//! the developer tools do, and outlives the worker being stopped and started again, so captured logs
//! continue across restarts. A reload installs a new worker, whose session replaces the old one.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::browser::{Browser, TargetEvent, TargetInfo};
use crate::cdp::{Cdp, Event};
use crate::cli::LoadState;
use crate::console::{Entry, Level, Log};
use crate::profiles::Profile;
use crate::undo::Undo;

use super::catalog::{self, Background, Extension};

/// Longest wait for a service worker to appear after starting it or reloading its extension, or
/// to disappear after stopping it.
const WORKER_WAIT: Duration = Duration::from_secs(5);

/// Interval between target-list checks while waiting: hba's target discovery does not report
/// workers, so their coming and going is polled.
const POLL: Duration = Duration::from_millis(25);

/// Longest wait for queued events of extension sessions to be recorded.
const FLUSH: Duration = Duration::from_secs(1);

/// Longest wait for a helper tab to load `chrome://extensions`.
const HELPER_LOAD: Duration = Duration::from_secs(10);

/// What an extension target is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Worker,
    BackgroundPage,
    /// Popup, options and other extension documents.
    Page,
}

impl Kind {
    pub fn of(target: &TargetInfo) -> Option<Self> {
        match target.kind.as_str() {
            "service_worker" => Some(Self::Worker),
            "background_page" => Some(Self::BackgroundPage),
            "page" | "iframe" | "other" => Some(Self::Page),
            _ => None,
        }
    }
}

/// The first target of `kind` among an extension's targets.
pub fn running(targets: &[TargetInfo], kind: Kind) -> Option<&TargetInfo> {
    targets.iter().find(|target| Kind::of(target) == Some(kind))
}

/// The targets of extension `id` among `targets`, in browser context `context` when given.
fn select(targets: &[TargetInfo], id: &str, context: Option<&str>) -> Vec<TargetInfo> {
    let origin = catalog::origin(id);
    let in_context = |target: &TargetInfo| {
        context.is_none_or(|context| target.browser_context_id.as_deref().is_none_or(|own| own == context))
    };
    targets.iter().filter(|target| target.url.starts_with(&origin) && in_context(target)).cloned().collect()
}

#[derive(Debug)]
struct Attached {
    session: Arc<str>,
    extension: String,
    kind: Kind,
    /// `Runtime` and `Log` are enabled for log capture.
    logging: bool,
}

#[derive(Debug)]
struct Feed {
    events: mpsc::UnboundedSender<Event>,
    barriers: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

/// Browser-wide extension debugging state: sessions on extension targets and captured consoles.
#[derive(Debug, Default)]
pub struct State {
    /// By target id.
    sessions: HashMap<String, Attached>,
    /// Console of each extension (by id) whose capture `hba ext logs` started.
    logs: HashMap<String, Log>,
    /// Delivers the sessions' events to the recording task, started with the first session.
    feed: Option<Feed>,
    /// Serializes attaching, so concurrent commands do not attach to one target twice.
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl State {
    /// Records an event of an extension session. Returns the session to drop when its page left
    /// the extension: `Runtime` must not stay enabled on a website.
    fn record(&mut self, event: &Event) -> Option<Arc<str>> {
        let session = event.session.as_deref()?;
        let (target, attached) = self.sessions.iter().find(|(_, attached)| &*attached.session == session)?;
        if attached.kind == Kind::Page && left_extension(event, target, &attached.extension) {
            let target = target.clone();
            return self.sessions.remove(&target).map(|attached| attached.session);
        }
        if let Some(log) = self.logs.get_mut(&attached.extension) {
            record(log, attached.kind, event);
        }
        None
    }

    fn session_of(&self, target: &str) -> Option<Arc<str>> {
        self.sessions.get(target).map(|attached| Arc::clone(&attached.session))
    }

    /// Removes the sessions `stale` selects and returns them for detaching.
    fn take(&mut self, stale: impl Fn(&str, &Attached) -> bool) -> Vec<Arc<str>> {
        let targets: Vec<String> = self
            .sessions
            .iter()
            .filter(|(target, attached)| stale(target, attached))
            .map(|(target, _)| target.clone())
            .collect();
        targets.iter().filter_map(|target| self.sessions.remove(target)).map(|attached| attached.session).collect()
    }
}

/// Whether a page session's main frame is leaving the extension: a navigation to another URL
/// starts (before the new document loads), or, as a fallback, the document is already there.
fn left_extension(event: &Event, target: &str, extension: &str) -> bool {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Navigation {
        frame_id: String,
        url: String,
        /// `currentTab`, or where else a requested navigation opens.
        #[serde(default)]
        disposition: Option<String>,
    }
    #[derive(Deserialize)]
    struct Created {
        context: Described,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Described {
        origin: String,
        #[serde(default)]
        aux_data: Option<Aux>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Aux {
        #[serde(default)]
        is_default: bool,
        #[serde(default)]
        frame_id: String,
    }
    // A page target's id is its main frame's id.
    match event.method.as_str() {
        "Page.frameStartedNavigating" | "Page.frameRequestedNavigation" => {
            event.parse::<Navigation>().is_ok_and(|navigation| {
                let url = navigation.url.strip_prefix("blob:").unwrap_or(&navigation.url);
                navigation.frame_id == target
                    && navigation.disposition.as_deref().is_none_or(|place| place == "currentTab")
                    && extension_of(url) != Some(extension)
            })
        }
        "Runtime.executionContextCreated" => event.parse::<Created>().is_ok_and(|Created { context }| {
            let main_document = context.aux_data.is_some_and(|aux| aux.is_default && aux.frame_id == target);
            main_document && context.origin.strip_prefix("chrome-extension://") != Some(extension)
        }),
        _ => false,
    }
}

/// Adds an event of an extension session to its log: console messages as `hba console` records
/// them, plus the worker stopping and starting.
fn record(log: &mut Log, kind: Kind, event: &Event) {
    match event.method.as_str() {
        // Sent to a worker's session when the worker stops and when it starts again.
        "Inspector.targetCrashed" if kind == Kind::Worker => {
            log.push(Level::Info, "[service worker stopped]".to_owned(), String::new(), false);
        }
        "Inspector.targetReloadedAfterCrash" if kind == Kind::Worker => {
            log.push(Level::Info, "[service worker started]".to_owned(), String::new(), false);
        }
        _ => crate::console::record(log, event),
    }
}

/// The extension id of a `chrome-extension://ID/…` URL.
fn extension_of(url: &str) -> Option<&str> {
    let id = url.strip_prefix("chrome-extension://")?.split('/').next()?;
    catalog::is_id(id).then_some(id)
}

/// Records events of extension sessions, attaches capture to new pages of captured extensions,
/// forgets sessions of closed pages, and answers flush barriers once earlier events are recorded.
async fn pump(
    browser: Weak<Browser>,
    mut events: mpsc::UnboundedReceiver<Event>,
    mut barriers: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
    mut targets: broadcast::Receiver<TargetEvent>,
) {
    loop {
        tokio::select! {
            biased;
            event = events.recv() => {
                let (Some(event), Some(browser)) = (event, browser.upgrade()) else { return };
                let left = browser.extensions.lock().expect("extensions lock").record(&event);
                if let Some(session) = left {
                    detach(&browser.cdp, &session).await;
                }
            }
            barrier = barriers.recv() => {
                let Some(done) = barrier else { return };
                let _ = done.send(());
            }
            change = targets.recv() => match change {
                Ok(TargetEvent::Created(info)) => {
                    if let Some(browser) = browser.upgrade() {
                        adopt_page(&browser, info);
                    }
                }
                Ok(TargetEvent::Destroyed(target)) => {
                    let Some(browser) = browser.upgrade() else { return };
                    let gone = browser.extensions.lock().expect("extensions lock").sessions.remove(&target);
                    if let Some(gone) = gone {
                        browser.cdp.unroute(&gone.session);
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
        }
    }
}

/// Attaches log capture to a new page of an extension whose console is captured.
fn adopt_page(browser: &Arc<Browser>, info: Arc<TargetInfo>) {
    let Some(id) = extension_of(&info.url).map(str::to_owned) else {
        return;
    };
    let gate = {
        let state = browser.extensions.lock().expect("extensions lock");
        if !state.logs.contains_key(&id)
            || state.sessions.contains_key(&info.target_id)
            || Kind::of(&info) != Some(Kind::Page)
        {
            return;
        }
        Arc::clone(&state.gate)
    };
    let browser = Arc::clone(browser);
    tokio::spawn(async move {
        let _gate = gate.lock().await;
        let attached = browser.extensions.lock().expect("extensions lock").sessions.contains_key(&info.target_id);
        if !attached {
            // The page may close before hba attaches; the next `hba ext logs` looks again.
            attach(&browser, &info, &id, Kind::Page, true).await.ok();
        }
    });
}

/// The sender feeding the recording task, starting the task on first use.
fn feed(browser: &Arc<Browser>) -> mpsc::UnboundedSender<Event> {
    let mut state = browser.extensions.lock().expect("extensions lock");
    if let Some(feed) = state.feed.as_ref().filter(|feed| !feed.events.is_closed()) {
        return feed.events.clone();
    }
    let (events, events_rx) = mpsc::unbounded_channel();
    let (barriers, barriers_rx) = mpsc::unbounded_channel();
    tokio::spawn(pump(Arc::downgrade(browser), events_rx, barriers_rx, browser.subscribe()));
    state.feed = Some(Feed { events: events.clone(), barriers });
    events
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Created {
    target_id: String,
}

/// Attaches to an extension target and keeps the session; with `logging`, enables `Runtime` and
/// `Log`, which replay what the target logged before.
async fn attach(
    browser: &Arc<Browser>,
    target: &TargetInfo,
    extension: &str,
    kind: Kind,
    logging: bool,
) -> Result<Arc<str>> {
    let attached: crate::page::Attached = browser
        .cdp
        .call("Target.attachToTarget", json!({"targetId": target.target_id, "flatten": true}), None)
        .await
        .with_context(|| format!("attaching to {}", target.url))?;
    let session: Arc<str> = Arc::from(attached.session_id);
    browser.cdp.route(&session, feed(browser));
    // A worker started while a debugger was expected waits for one; this attach is it. Resumed
    // that early, it gets its extension APIs a moment later.
    browser.cdp.send("Runtime.runIfWaitingForDebugger", json!({}), Some(&session)).await.ok();
    if kind == Kind::Page {
        // Reports the main frame's navigations as they start, so the session is dropped before a
        // website's document loads with `Runtime` on (`Page` is invisible to pages).
        browser.cdp.send("Page.enable", json!({}), Some(&session)).await.ok();
    }
    if kind == Kind::Worker {
        let ready = "typeof chrome === 'object' && !!chrome.runtime";
        let deadline = Instant::now() + WORKER_WAIT;
        while crate::page::evaluate_on(&browser.cdp, &session, None, ready, false, WORKER_WAIT).await.ok()
            != Some(Value::Bool(true))
            && Instant::now() < deadline
        {
            tokio::time::sleep(POLL).await;
        }
    }
    let replaced = browser.extensions.lock().expect("extensions lock").sessions.insert(
        target.target_id.clone(),
        Attached { session: Arc::clone(&session), extension: extension.to_owned(), kind, logging },
    );
    if let Some(replaced) = replaced {
        detach(&browser.cdp, &replaced.session).await;
    }
    if logging && let Err(error) = enable_logging(&browser.cdp, &session).await {
        browser.extensions.lock().expect("extensions lock").sessions.remove(&target.target_id);
        detach(&browser.cdp, &session).await;
        return Err(error);
    }
    Ok(session)
}

async fn enable_logging(cdp: &Cdp, session: &str) -> Result<()> {
    let (runtime, log) = tokio::join!(
        cdp.send("Runtime.enable", json!({}), Some(session)),
        cdp.send("Log.enable", json!({}), Some(session)),
    );
    // Workers lack the Log domain; their console comes through Runtime.
    log.ok();
    runtime
}

async fn detach(cdp: &Cdp, session: &str) {
    cdp.unroute(session);
    cdp.send("Target.detachFromTarget", json!({"sessionId": session}), None).await.ok();
}

/// What starting log capture found.
#[derive(Debug)]
pub struct Capture {
    /// Capture began with this call.
    pub started: bool,
    /// hba started the stopped service worker to capture it.
    pub woke: bool,
    /// A worker or background page is captured.
    pub worker: bool,
    /// Extension pages captured.
    pub pages: usize,
}

/// Extension debugging in one profile of the connected browser.
#[derive(Clone, Copy)]
pub struct Debugger<'a> {
    pub browser: &'a Arc<Browser>,
    pub profile: &'a Profile,
    /// The profile's browser context, when hba has seen it.
    pub context: Option<&'a str>,
    /// `profile` is Helium's default profile, where helper pages open without a known context.
    pub default_profile: bool,
}

impl Debugger<'_> {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.browser.extensions.lock().expect("extensions lock")
    }

    fn gate(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.state().gate)
    }

    /// The targets of extension `id` among `targets`, in the profile when its context is known.
    pub fn select(&self, targets: &[TargetInfo], id: &str) -> Vec<TargetInfo> {
        select(targets, id, self.context)
    }

    /// Live targets of extension `id`.
    pub async fn targets(&self, id: &str) -> Result<Vec<TargetInfo>> {
        Ok(self.select(&self.browser.targets().await?, id))
    }

    pub fn capturing(&self, id: &str) -> bool {
        self.state().logs.contains_key(id)
    }

    /// Whether hba holds a session on the extension's worker or background page.
    pub fn worker_attached(&self, id: &str) -> bool {
        self.state().sessions.values().any(|attached| attached.extension == id && attached.kind != Kind::Page)
    }

    /// A session on the service worker or background page, starting a stopped worker.
    pub async fn worker(&self, ext: &Extension) -> Result<Arc<str>> {
        let name = ext.name();
        let kind = match ext.manifest.background {
            Background::Worker => Kind::Worker,
            Background::Page => Kind::BackgroundPage,
            Background::None => bail!(
                "{name} has no service worker or background page; `hba js --world {:?} CODE` runs in its content scripts",
                ext.id
            ),
        };
        if !ext.enabled {
            bail!("{name} is disabled");
        }
        let gate = self.gate();
        let _gate = gate.lock().await;
        let targets = self.targets(&ext.id).await?;
        self.drop_stale(&ext.id, &targets).await;
        let target = match running(&targets, kind) {
            Some(target) => target.clone(),
            None if kind == Kind::Worker => self.wake(ext).await?,
            None => bail!("{name}'s background page is not running; `hba ext reload {:?}` restarts it", ext.id),
        };
        let known = self.state().session_of(&target.target_id);
        let session = match known {
            Some(session) => session,
            None => attach(self.browser, &target, &ext.id, kind, self.capturing(&ext.id)).await?,
        };
        Ok(session)
    }

    /// A session `Extensions.*StorageItems` accepts: the worker or background page, else an open
    /// extension page (hidden helper pages are refused).
    pub async fn storage_session(&self, ext: &Extension) -> Result<Arc<str>> {
        if !ext.enabled {
            bail!("{} is disabled", ext.name());
        }
        if ext.manifest.background != Background::None {
            return self.worker(ext).await;
        }
        let gate = self.gate();
        let _gate = gate.lock().await;
        let targets = self.targets(&ext.id).await?;
        let Some(page) = targets.iter().find(|target| target.kind == "page") else {
            bail!(
                "{} has no service worker and no open page to reach chrome.storage through; open one first: `hba ext open {:?} manifest.json`",
                ext.name(),
                ext.id
            );
        };
        let known = self.state().session_of(&page.target_id);
        match known {
            Some(session) => Ok(session),
            None => attach(self.browser, page, &ext.id, Kind::Page, self.capturing(&ext.id)).await,
        }
    }

    /// Starts the stopped service worker without attaching; `false` when it was running.
    pub async fn start_worker(&self, ext: &Extension) -> Result<bool> {
        if !ext.enabled {
            bail!("{} is disabled", ext.name());
        }
        let gate = self.gate();
        let _gate = gate.lock().await;
        if running(&self.targets(&ext.id).await?, Kind::Worker).is_some() {
            return Ok(false);
        }
        self.wake(ext).await.map(|_| true)
    }

    /// Starts the stopped service worker from a helper page and returns its target.
    async fn wake(&self, ext: &Extension) -> Result<TargetInfo> {
        let helper = Helper::open(self, "about:blank").await?;
        let cdp = &self.browser.cdp;
        let started = async {
            cdp.send("ServiceWorker.enable", json!({}), Some(&helper.session)).await?;
            cdp.send("ServiceWorker.startWorker", json!({"scopeURL": ext.origin()}), Some(&helper.session))
                .await
                .with_context(|| format!("starting {}'s service worker", ext.name()))?;
            self.wait_worker(ext, &[]).await
        }
        .await;
        helper.close().await;
        started
    }

    /// Waits for a running service worker other than the `old` targets.
    pub async fn wait_worker(&self, ext: &Extension, old: &[String]) -> Result<TargetInfo> {
        let deadline = Instant::now() + WORKER_WAIT;
        loop {
            let targets = self.targets(&ext.id).await?;
            let fresh = targets
                .into_iter()
                .find(|target| Kind::of(target) == Some(Kind::Worker) && !old.contains(&target.target_id));
            if let Some(worker) = fresh {
                return Ok(worker);
            }
            if Instant::now() >= deadline {
                bail!(
                    "{}'s service worker did not start within {}s; `hba ext errors {:?}` shows why",
                    ext.name(),
                    WORKER_WAIT.as_secs(),
                    ext.id
                );
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Terminates the running service worker; `false` when it was stopped already.
    pub async fn stop_worker(&self, ext: &Extension) -> Result<bool> {
        let gate = self.gate();
        let _gate = gate.lock().await;
        let Some(worker) = running(&self.targets(&ext.id).await?, Kind::Worker).cloned() else {
            return Ok(false);
        };
        self.browser.cdp.send("Target.closeTarget", json!({"targetId": worker.target_id}), None).await?;
        let deadline = Instant::now() + WORKER_WAIT;
        while self.targets(&ext.id).await?.iter().any(|target| target.target_id == worker.target_id) {
            if Instant::now() >= deadline {
                bail!("{}'s service worker is still running {}s after stopping it", ext.name(), WORKER_WAIT.as_secs());
            }
            tokio::time::sleep(POLL).await;
        }
        if !self.capturing(&ext.id) {
            // Nothing needs the session now; without it the worker idles out after its next start.
            let session = self.state().sessions.remove(&worker.target_id).map(|attached| attached.session);
            if let Some(session) = session {
                detach(&self.browser.cdp, &session).await;
            }
        }
        Ok(true)
    }

    /// Starts capturing the extension's console (once) and attaches it to every target: the
    /// worker (started when stopped, the first time), background page and extension pages.
    pub async fn capture(&self, ext: &Extension) -> Result<Capture> {
        let gate = self.gate();
        let guard = gate.lock().await;
        let started = {
            let mut state = self.state();
            let started = !state.logs.contains_key(&ext.id);
            state.logs.entry(ext.id.clone()).or_default();
            started
        };
        let mut targets = self.targets(&ext.id).await?;
        let first_worker_capture = started && ext.enabled && ext.manifest.background == Background::Worker;
        let mut woke = false;
        if first_worker_capture && running(&targets, Kind::Worker).is_none() {
            match self.wake(ext).await {
                Ok(worker) => targets.push(worker),
                Err(error) => {
                    self.state().logs.remove(&ext.id);
                    return Err(error);
                }
            }
            woke = true;
        }
        self.drop_stale(&ext.id, &targets).await;
        for target in &targets {
            let Some(kind) = Kind::of(target) else {
                continue;
            };
            let known = self
                .state()
                .sessions
                .get(&target.target_id)
                .map(|attached| (Arc::clone(&attached.session), attached.logging));
            let outcome = match known {
                Some((_, true)) => Ok(()),
                Some((session, false)) => enable_logging(&self.browser.cdp, &session).await.map(|()| {
                    if let Some(attached) = self.state().sessions.get_mut(&target.target_id) {
                        attached.logging = true;
                    }
                }),
                None => attach(self.browser, target, &ext.id, kind, true).await.map(drop),
            };
            // Pages come and go at any moment; only the worker must be reachable.
            if kind != Kind::Page {
                outcome?;
            }
        }
        drop(guard);
        self.flush().await;
        let worker = targets.iter().any(|target| Kind::of(target).is_some_and(|kind| kind != Kind::Page));
        let pages = targets.iter().filter(|target| Kind::of(target) == Some(Kind::Page)).count();
        Ok(Capture { started, woke, worker, pages })
    }

    /// Detaches sessions of the extension's closed pages and replaced workers. A stopped worker
    /// is not listed, but its session resumes when it starts again, so it stays.
    async fn drop_stale(&self, id: &str, targets: &[TargetInfo]) {
        let worker = running(targets, Kind::Worker).map(|target| target.target_id.clone());
        let stale = self.state().take(|target, attached| {
            attached.extension == id
                && match attached.kind {
                    Kind::Worker => worker.as_deref().is_some_and(|live| live != target),
                    Kind::BackgroundPage | Kind::Page => !targets.iter().any(|live| live.target_id == target),
                }
        });
        for session in stale {
            detach(&self.browser.cdp, &session).await;
        }
    }

    /// Waits until events already received from extension sessions are recorded.
    async fn flush(&self) {
        let barriers = self.state().feed.as_ref().map(|feed| feed.barriers.clone());
        let Some(barriers) = barriers else { return };
        let (done, recorded) = oneshot::channel();
        if barriers.send(done).is_ok() {
            let _ = tokio::time::timeout(FLUSH, recorded).await;
        }
    }

    /// The last `last` captured console entries, after how many earlier ones are left out.
    pub fn entries(&self, id: &str, last: usize) -> (usize, Vec<Entry>) {
        let state = self.state();
        let Some(log) = state.logs.get(id) else { return (0, Vec::new()) };
        let earlier = log.entries.len().saturating_sub(last);
        (earlier, log.entries.iter().skip(earlier).cloned().collect())
    }

    /// Forgets captured entries; returns how many there were.
    pub fn clear(&self, id: &str) -> usize {
        self.state().logs.get_mut(id).map_or(0, |log| {
            let count = log.entries.len();
            log.clear();
            count
        })
    }

    /// Detaches every session of the extension, keeping its captured console.
    pub async fn release(&self, id: &str) {
        let sessions = self.state().take(|_, attached| attached.extension == id);
        for session in sessions {
            detach(&self.browser.cdp, &session).await;
        }
    }

    /// Detaches every session of the extension and drops its captured console.
    pub async fn forget(&self, id: &str) {
        self.state().logs.remove(id);
        self.release(id).await;
    }
}

/// A short-lived page for browser-side work: hidden from the tab strip where Helium allows it
/// (in its default profile), else a background tab of the profile. Dropping it closes the page,
/// so a command that is cancelled does not leave it behind.
pub struct Helper {
    pub session: Arc<str>,
    close: Undo,
}

impl Helper {
    /// Opens `url` (`about:blank` or a browser page) in the debugger's profile.
    pub async fn open(debugger: &Debugger<'_>, url: &str) -> Result<Self> {
        let browser = debugger.browser;
        if debugger.context.is_some() || debugger.default_profile {
            let mut params = json!({"url": "about:blank", "hidden": true, "background": true});
            if let Some(context) = debugger.context {
                params["browserContextId"] = json!(context);
            }
            match browser.cdp.call::<Created>("Target.createTarget", params, None).await {
                Ok(created) => return Self::hidden(browser, &created.target_id, url).await,
                // Helium creates targets only in its default profile; elsewhere a tab stands in.
                Err(error) if format!("{error:#}").contains("Failed to find browser context") => {}
                Err(error) => return Err(error.context("opening a hidden helper page")),
            }
        }
        let tab = browser.create_tab(debugger.profile, true).await?;
        let close = closing(browser, &tab);
        let page = browser.attach(&tab).await?;
        if url != "about:blank" {
            page.goto(url, LoadState::Load, HELPER_LOAD).await?;
        }
        Ok(Self { session: page.session()?, close })
    }

    async fn hidden(browser: &Arc<Browser>, target: &str, url: &str) -> Result<Self> {
        let close = closing(browser, target);
        let attached: crate::page::Attached =
            browser.cdp.call("Target.attachToTarget", json!({"targetId": target, "flatten": true}), None).await?;
        if url != "about:blank" {
            // Answered once the navigation commits, so the new document is ready for evaluation.
            browser.cdp.send("Page.navigate", json!({"url": url}), Some(&attached.session_id)).await?;
        }
        Ok(Self { session: Arc::from(attached.session_id), close })
    }

    pub async fn close(self) {
        self.close.finish().await;
    }
}

/// Closes `target` once finished or dropped.
fn closing(browser: &Arc<Browser>, target: &str) -> Undo {
    let (browser, target) = (Arc::clone(browser), target.to_owned());
    Undo::new(async move {
        browser.close_target(&target).await.ok();
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use serde_json::value::RawValue;

    use super::*;

    const ID: &str = "ilidgonjdoemebfgciljbbklchfokkln";

    fn event(method: &str, params: &Value) -> Event {
        Event {
            session: Some(Arc::from("S1")),
            method: method.to_owned(),
            params: RawValue::from_string(params.to_string()).expect("valid JSON"),
        }
    }

    fn summary(log: &Log) -> Vec<(Level, String, String, bool)> {
        log.entries.iter().map(|e| (e.level, e.text.clone(), e.location.clone(), e.exception)).collect()
    }

    #[test]
    fn console_messages_and_worker_lifecycle() {
        let mut log = Log::default();
        let url = format!("chrome-extension://{ID}/sw.js");
        let console = json!({"type": "warning", "args": [{"type": "string", "value": "slow"}, {"type": "number", "value": 3}],
                             "stackTrace": {"callFrames": [{"url": url, "lineNumber": 4}]}});
        record(&mut log, Kind::Worker, &event("Runtime.consoleAPICalled", &console));
        record(&mut log, Kind::Worker, &event("Inspector.targetCrashed", &json!({})));
        record(&mut log, Kind::Page, &event("Inspector.targetCrashed", &json!({})));
        record(&mut log, Kind::Worker, &event("Inspector.targetReloadedAfterCrash", &json!({})));
        assert_eq!(
            summary(&log),
            [
                (Level::Warn, "slow 3".to_owned(), "sw.js:5".to_owned(), false),
                (Level::Info, "[service worker stopped]".to_owned(), String::new(), false),
                (Level::Info, "[service worker started]".to_owned(), String::new(), false),
            ],
            "console messages as `hba console` records them; lifecycle of workers only"
        );
    }

    #[test]
    fn page_sessions_leave_with_their_document() {
        let page = "PAGE1";
        let created = |origin: &str, frame: &str, default: bool| {
            event(
                "Runtime.executionContextCreated",
                &json!({"context": {"id": 1, "origin": origin, "name": "",
                                    "auxData": {"isDefault": default, "type": "default", "frameId": frame}}}),
            )
        };
        assert!(left_extension(&created("https://example.com", page, true), page, ID), "main document on a website");
        assert!(!left_extension(&created(&format!("chrome-extension://{ID}"), page, true), page, ID));
        assert!(!left_extension(&created("https://example.com", "CHILD", true), page, ID), "an iframe of the page");
        assert!(!left_extension(&created("https://example.com", page, false), page, ID), "an isolated world");
    }

    #[test]
    fn page_sessions_leave_as_soon_as_a_navigation_away_starts() {
        let page = "PAGE1";
        let started = |url: &str, frame: &str| {
            event(
                "Page.frameStartedNavigating",
                &json!({"frameId": frame, "url": url, "loaderId": "L", "navigationType": "differentDocument"}),
            )
        };
        let requested = |url: &str, disposition: &str| {
            event(
                "Page.frameRequestedNavigation",
                &json!({"frameId": page, "reason": "scriptInitiated", "url": url, "disposition": disposition}),
            )
        };
        assert!(left_extension(&started("https://example.com/", page), page, ID));
        assert!(left_extension(&started("about:blank", page), page, ID));
        assert!(!left_extension(&started(&format!("chrome-extension://{ID}/options.html#a"), page), page, ID));
        assert!(!left_extension(&started(&format!("blob:chrome-extension://{ID}/9d1c"), page), page, ID));
        assert!(!left_extension(&started("https://example.com/", "CHILD"), page, ID), "an iframe navigating");
        assert!(left_extension(&requested("https://example.com/", "currentTab"), page, ID));
        assert!(!left_extension(&requested("https://example.com/", "newTab"), page, ID), "opens elsewhere");
    }

    #[test]
    fn state_routes_events_to_the_extension_log() {
        let mut state = State::default();
        state.sessions.insert(
            "PAGE1".to_owned(),
            Attached { session: Arc::from("S1"), extension: ID.to_owned(), kind: Kind::Page, logging: true },
        );
        state.logs.insert(ID.to_owned(), Log::default());
        let log =
            event("Runtime.consoleAPICalled", &json!({"type": "log", "args": [{"type": "string", "value": "hi"}]}));
        assert!(state.record(&log).is_none());
        assert_eq!(state.logs[ID].entries.len(), 1);
        let away = event(
            "Runtime.executionContextCreated",
            &json!({"context": {"id": 2, "origin": "https://example.com", "auxData": {"isDefault": true, "frameId": "PAGE1"}}}),
        );
        assert_eq!(state.record(&away).as_deref(), Some("S1"), "the session is handed back for detaching");
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn extension_urls() {
        assert_eq!(extension_of(&format!("chrome-extension://{ID}/popup.html")), Some(ID));
        assert_eq!(extension_of("chrome-extension://short/popup.html"), None);
        assert_eq!(extension_of("https://example.com/"), None);
    }

    #[test]
    fn target_kinds_and_selection() {
        let target = |id: &str, kind: &str, url: &str, context: &str| TargetInfo {
            target_id: id.to_owned(),
            kind: kind.to_owned(),
            title: String::new(),
            url: url.to_owned(),
            opener_id: None,
            browser_context_id: Some(context.to_owned()),
            subtype: None,
            embedder_data: None,
        };
        let own = format!("chrome-extension://{ID}/");
        let targets = [
            target("W", "service_worker", &format!("{own}sw.js"), "C1"),
            target("P", "page", &format!("{own}popup.html"), "C1"),
            target("D", "worker", &format!("{own}worker.js"), "C1"),
            target("X", "page", &format!("{own}options.html"), "C2"),
            target("Y", "page", "https://example.com/", "C1"),
        ];
        assert_eq!(
            targets.iter().map(Kind::of).collect::<Vec<_>>(),
            [Some(Kind::Worker), Some(Kind::Page), None, Some(Kind::Page), Some(Kind::Page)]
        );
        assert_eq!(running(&targets, Kind::Worker).map(|t| t.target_id.as_str()), Some("W"));
        let ids =
            |context: Option<&str>| select(&targets, ID, context).into_iter().map(|t| t.target_id).collect::<Vec<_>>();
        assert_eq!(ids(Some("C1")), ["W", "P", "D"], "the profile's targets of this extension");
        assert_eq!(ids(None), ["W", "P", "D", "X"], "any profile when its context is unknown");
    }
}
