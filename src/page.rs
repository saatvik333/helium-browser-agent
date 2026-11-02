//! One attached tab: its page and frame sessions, isolated worlds, navigation and dialogs.
//!
//! Attachment goes through the tab target, so the page survives prerender activations and
//! process swaps. `Runtime` stays disabled on websites (it is an automation tell); `hba`'s own
//! code runs in an isolated world and page JavaScript runs without enabling any domain.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, watch};

use crate::cdp::{Cdp, Event};
use crate::cli::LoadState;
use crate::config::Humanize;
use crate::injected::Source;
use crate::undo::Undo;

/// The DOM counts as settled after this long without mutations or new requests.
const QUIET: Duration = Duration::from_millis(100);

/// Longest an action waits for the page to settle when nothing navigates.
const SETTLE_CAP: Duration = Duration::from_millis(1500);

/// Notable page changes, for waiters.
#[derive(Debug, Clone)]
pub enum Signal {
    NavigationStarted,
    Committed,
    Lifecycle,
    Dialog,
    FileChooser { backend_node_id: i64, multiple: bool },
    DragIntercepted(Value),
    Download,
    Detached,
}

#[derive(Debug, Clone)]
pub struct Dialog {
    pub kind: String,
    pub message: String,
    pub default_prompt: String,
}

/// A frame and the session that hosts it.
#[derive(Debug, Clone)]
struct Frame {
    session: Arc<str>,
    parent: Option<String>,
    seq: u32,
    world: Option<World>,
}

/// `hba`'s isolated world in one frame, with the Playwright engine instance.
#[derive(Debug, Clone)]
pub struct World {
    pub session: Arc<str>,
    pub frame: String,
    pub context: i64,
    pub engine: String,
}

/// A JavaScript context reported while `Runtime` is enabled (page, isolated or extension world).
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub id: i64,
    pub session: Arc<str>,
    pub origin: String,
    pub name: String,
    /// `default`, `isolated` or `worker`.
    pub kind: String,
    pub frame: Option<String>,
}

/// A resolved element: a remote object in `world`.
#[derive(Debug, Clone)]
pub struct Element {
    pub world: World,
    pub object: String,
    /// Short description for messages, e.g. `button "Save"`.
    pub label: String,
}

/// Argument to [`Page::call`].
pub enum Arg<'a> {
    Value(Value),
    Object(&'a str),
}

#[derive(Default)]
struct State {
    primary: Option<Arc<str>>,
    page_target: Option<String>,
    /// Page targets attached under the tab: target id → (session, is prerender).
    pages: HashMap<String, (Arc<str>, bool)>,
    main_frame: Option<String>,
    url: String,
    loader: Option<String>,
    /// Lifecycle events per main-frame loader, newest last. Prerendered and back/forward-cached
    /// documents load before they become current, so events are kept for a few loaders.
    lifecycles: Vec<(String, HashSet<String>)>,
    frames: HashMap<String, Frame>,
    next_seq: u32,
    navigations: u64,
    /// Main-frame navigations started (not necessarily committed).
    starts: u64,
    /// URL the main frame is navigating to, from its start (or redirect) until it commits or
    /// the frame stops loading without committing (a download, a 204 response).
    pending: Option<String>,
    /// The pending navigation goes through history. One served from the back/forward cache
    /// stops loading just before it commits.
    history: bool,
    dialog: Option<Dialog>,
    /// Alerts accepted automatically since the last report.
    notes: Vec<String>,
    mouse: (f64, f64),
    /// Mouse buttons held down (CDP `buttons` bitmask).
    buttons: u8,
    /// Snapshot memory for `snap -d` and stale-ref messages.
    last_snapshot: Option<(Option<String>, Vec<String>)>,
    contexts: Vec<ExecutionContext>,
    /// Frame → world context of its last snapshot. Refs belong to one document; a frame that
    /// navigated since restarts numbering, so its old refs must not resolve.
    snapshotted: HashMap<String, i64>,
}

impl State {
    /// Whether the current document has fired lifecycle event `name`.
    fn fired(&self, name: &str) -> bool {
        let Some(loader) = &self.loader else { return false };
        self.lifecycles.iter().any(|(seen, names)| seen == loader && names.contains(name))
    }

    fn record_lifecycle(&mut self, loader: &str, names: &[&str]) {
        let names = names.iter().map(|name| (*name).to_owned());
        if let Some((_, seen)) = self.lifecycles.iter_mut().find(|(seen, _)| seen == loader) {
            seen.extend(names);
            return;
        }
        if self.lifecycles.len() == 8 {
            self.lifecycles.remove(0);
        }
        self.lifecycles.push((loader.to_owned(), names.collect()));
    }
}

pub struct Page {
    /// This page, for tasks that outlive a call.
    me: Weak<Page>,
    pub tab: String,
    pub cdp: Cdp,
    source: Arc<Source>,
    pub humanize: Humanize,
    tab_session: String,
    /// The event loop's channel. Only routed sessions hold it strongly: once every session is
    /// unrouted the channel closes and the loop ends, freeing the page.
    events: mpsc::WeakUnboundedSender<Event>,
    state: Mutex<State>,
    signals: broadcast::Sender<Signal>,
    ready: watch::Sender<bool>,
    live: AtomicBool,
    pub console: Mutex<crate::console::Log>,
    pub network: Mutex<crate::network::Log>,
    pub downloads: Mutex<crate::download::Log>,
    pub routes: Mutex<crate::routes::Rules>,
    pub emulation: Mutex<crate::emulate::State>,
    /// Traces, profiles, coverage and metric readings that span two commands.
    pub devtools: Mutex<crate::devtools::State>,
    /// Permission overrides `hba perms grant` made, restored on release.
    pub permissions: Mutex<crate::storage::Grants>,
}

/// The reply of `Target.attachToTarget`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Attached {
    pub(crate) session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachedEvent {
    session_id: String,
    target_info: crate::browser::TargetInfo,
}

// Payloads of the events a page handles.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetachedEvent {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameAttached {
    frame_id: String,
    parent_frame_id: Option<String>,
}

#[derive(Deserialize)]
struct FrameNavigated {
    frame: FrameData,
    /// `Navigation` or `BackForwardCacheRestore`.
    #[serde(default, rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameDetached {
    frame_id: String,
    /// `remove`, or `swap` when the frame moves to another process.
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NavigatedWithinDocument {
    frame_id: String,
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameStarting {
    frame_id: String,
    url: String,
    /// `currentTab`, `newTab`, `newWindow` or `download` (`frameRequestedNavigation` only).
    #[serde(default)]
    disposition: Option<String>,
    /// `differentDocument`, `historyDifferentDocument`, `reload`, … (`frameStartedNavigating` only).
    #[serde(default)]
    navigation_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameStopped {
    frame_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleEvent {
    frame_id: String,
    loader_id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DialogOpening {
    message: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    default_prompt: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileChooserOpened {
    #[serde(default)]
    backend_node_id: i64,
    mode: String,
}

#[derive(Deserialize)]
struct ContextCreated {
    context: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextDestroyed {
    execution_context_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameTree {
    frame_tree: FrameNode,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameNode {
    frame: FrameData,
    #[serde(default)]
    child_frames: Vec<FrameNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameData {
    id: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    loader_id: Option<String>,
    #[serde(default)]
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteObject {
    #[serde(default)]
    object_id: Option<String>,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    unserializable_value: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Evaluated {
    result: RemoteObject,
    #[serde(default)]
    exception_details: Option<ExceptionDetails>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExceptionDetails {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub exception: Option<Value>,
    #[serde(default)]
    pub line_number: i64,
    #[serde(default)]
    pub url: Option<String>,
}

impl ExceptionDetails {
    /// The thrown value's message without the stack.
    pub fn message(&self) -> String {
        let description = self.exception.as_ref().and_then(|e| {
            e.get("description").and_then(Value::as_str).or_else(|| e.get("value").and_then(Value::as_str))
        });
        let text = description.unwrap_or(&self.text);
        let mut lines = text.lines().take_while(|line| !line.trim_start().starts_with("at "));
        let first = lines.next().unwrap_or("").to_owned();
        lines.fold(first, |mut acc, line| {
            acc.push('\n');
            acc.push_str(line);
            acc
        })
    }
}

/// Snapshot of counters taken before an action, to report what it changed.
#[derive(Debug, Clone)]
pub struct Mark {
    pub navigations: u64,
    pub starts: u64,
    pub url: String,
    pub console_seq: u64,
    pub network_seq: u64,
    pub download_seq: u64,
    pub at: Instant,
}

impl Page {
    pub async fn attach(cdp: Cdp, source: Arc<Source>, humanize: Humanize, tab: &str) -> Result<Arc<Self>> {
        let attached: Attached = cdp
            .call("Target.attachToTarget", json!({"targetId": tab, "flatten": true}), None)
            .await
            .with_context(|| format!("attaching to tab {tab}"))?;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let events = events_tx.downgrade();
        cdp.route(&attached.session_id, events_tx);
        let (signals, _) = broadcast::channel(512);
        let (ready, mut ready_rx) = watch::channel(false);
        let page = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            tab: tab.to_owned(),
            cdp: cdp.clone(),
            source,
            humanize,
            tab_session: attached.session_id.clone(),
            events,
            state: Mutex::new(State {
                mouse: (fastrand::f64() * 200.0 + 100.0, fastrand::f64() * 120.0 + 80.0),
                ..State::default()
            }),
            signals,
            ready,
            live: AtomicBool::new(true),
            console: Mutex::default(),
            network: Mutex::default(),
            downloads: Mutex::default(),
            routes: Mutex::default(),
            emulation: Mutex::default(),
            devtools: Mutex::default(),
            permissions: Mutex::default(),
        });
        // A command cancelled before the page is ready lets go of the tab again.
        let undo = Undo::new({
            let page = Arc::clone(&page);
            async move { page.detach().await }
        });
        tokio::spawn(Arc::clone(&page).run(events_rx));
        // New pages (prerendering) wait until `prepare` has set them up; see there.
        let auto_attach = cdp
            .send(
                "Target.setAutoAttach",
                json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
                Some(&attached.session_id),
            )
            .await;
        let ready = auto_attach.is_ok()
            && matches!(
                tokio::time::timeout(Duration::from_secs(10), ready_rx.wait_for(|ready| *ready)).await,
                Ok(Ok(_))
            );
        if !ready {
            undo.finish().await;
            auto_attach?;
            bail!("tab {tab} did not expose a page to attach to");
        }
        undo.disarm();
        Ok(page)
    }

    /// The page target inside a tab target, by briefly attaching to the tab.
    pub async fn page_target_of(cdp: &Cdp, tab: &str) -> Result<Option<String>> {
        let attached: Attached =
            cdp.call("Target.attachToTarget", json!({"targetId": tab, "flatten": true}), None).await?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        cdp.route(&attached.session_id, tx);
        let undo = Undo::new({
            let (cdp, session) = (cdp.clone(), attached.session_id.clone());
            async move {
                cdp.unroute(&session);
                cdp.send("Target.detachFromTarget", json!({"sessionId": session}), None).await.ok();
            }
        });
        let result = async {
            cdp.send(
                "Target.setAutoAttach",
                json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
                Some(&attached.session_id),
            )
            .await?;
            let found = tokio::time::timeout(Duration::from_secs(2), async {
                while let Some(event) = rx.recv().await {
                    if event.method == "Target.attachedToTarget"
                        && let Ok(attached) = event.parse::<AttachedEvent>()
                        && attached.target_info.kind == "page"
                    {
                        return Some(attached.target_info.target_id);
                    }
                }
                None
            })
            .await;
            Ok::<_, anyhow::Error>(found.ok().flatten())
        }
        .await;
        undo.finish().await;
        result
    }

    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire) && !self.cdp.is_closed()
    }

    /// Marks the page gone and stops routing its sessions' events, which ends its event loop.
    pub fn mark_detached(&self) {
        self.live.store(false, Ordering::Release);
        let sessions: Vec<Arc<str>> = {
            let state = self.state.lock().expect("page lock");
            state
                .frames
                .values()
                .map(|frame| Arc::clone(&frame.session))
                .chain(state.pages.values().map(|p| Arc::clone(&p.0)))
                .collect()
        };
        for session in sessions.iter().chain(&crate::console::session(self)) {
            self.cdp.unroute(session);
        }
        self.cdp.unroute(&self.tab_session);
        let _ = self.signals.send(Signal::Detached);
    }

    /// Lets go of the tab, leaving it open.
    pub async fn detach(&self) {
        self.mark_detached();
        // hba's console session hangs off the browser, not the tab session.
        crate::console::release(self).await;
        self.cdp.send("Target.detachFromTarget", json!({"sessionId": self.tab_session}), None).await.ok();
    }

    /// This page as an `Arc`, for tasks that outlive a call.
    pub fn arc(&self) -> Arc<Self> {
        self.me.upgrade().expect("pages live in an Arc")
    }

    /// Delivers `session`'s events to this page; false once the page is detached.
    pub fn route(&self, session: &str) -> bool {
        let Some(sender) = self.events.upgrade() else { return false };
        self.cdp.route(session, sender);
        true
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Signal> {
        self.signals.subscribe()
    }

    pub fn signal(&self, signal: Signal) {
        let _ = self.signals.send(signal);
    }

    /// The primary page session.
    pub fn session(&self) -> Result<Arc<str>> {
        if !self.is_live() {
            bail!("the tab was closed");
        }
        self.state.lock().expect("page lock").primary.clone().context("the tab has no page right now; retry")
    }

    pub fn page_target(&self) -> Option<String> {
        self.state.lock().expect("page lock").page_target.clone()
    }

    pub fn url(&self) -> String {
        self.state.lock().expect("page lock").url.clone()
    }

    /// The current document's URL and the URL the main frame is navigating to, if any.
    pub fn urls(&self) -> (String, Option<String>) {
        let state = self.state.lock().expect("page lock");
        (state.url.clone(), state.pending.clone())
    }

    /// The main frame's document request was redirected to `url` before committing.
    pub fn on_document_redirect(&self, frame: &str, url: &str) {
        let mut state = self.state.lock().expect("page lock");
        if state.main_frame.as_deref() != Some(frame) || state.pending.is_none() {
            return;
        }
        state.pending = Some(url.to_owned());
        drop(state);
        crate::console::reconsider(self);
    }

    pub fn navigations(&self) -> u64 {
        self.state.lock().expect("page lock").navigations
    }

    pub fn main_frame(&self) -> Result<String> {
        self.state.lock().expect("page lock").main_frame.clone().context("the page has no main frame yet")
    }

    pub fn dialog(&self) -> Option<Dialog> {
        self.state.lock().expect("page lock").dialog.clone()
    }

    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().expect("page lock").notes)
    }

    pub fn mouse(&self) -> (f64, f64) {
        self.state.lock().expect("page lock").mouse
    }

    pub fn set_mouse(&self, point: (f64, f64)) {
        self.state.lock().expect("page lock").mouse = point;
    }

    /// Mouse buttons held down, as the CDP `buttons` bitmask.
    pub fn buttons(&self) -> u8 {
        self.state.lock().expect("page lock").buttons
    }

    pub fn set_buttons(&self, buttons: u8) {
        self.state.lock().expect("page lock").buttons = buttons;
    }

    /// Contexts seen while `Runtime` is enabled on the tab (none otherwise).
    pub fn execution_contexts(&self) -> Vec<ExecutionContext> {
        self.state.lock().expect("page lock").contexts.clone()
    }

    pub fn last_snapshot(&self) -> Option<(Option<String>, Vec<String>)> {
        self.state.lock().expect("page lock").last_snapshot.clone()
    }

    pub fn remember_snapshot(&self, lines: Vec<String>) {
        let mut state = self.state.lock().expect("page lock");
        state.last_snapshot = Some((state.loader.clone(), lines));
    }

    /// Records that refs in `world`'s frame now come from this document.
    pub fn note_snapshot(&self, world: &World) {
        self.state.lock().expect("page lock").snapshotted.insert(world.frame.clone(), world.context);
    }

    pub fn loader(&self) -> Option<String> {
        self.state.lock().expect("page lock").loader.clone()
    }

    /// Frame id for a ref prefix sequence (0 = main frame).
    pub fn frame_by_seq(&self, seq: u32) -> Option<String> {
        let state = self.state.lock().expect("page lock");
        if seq == 0 {
            return state.main_frame.clone();
        }
        state.frames.iter().find(|(_, frame)| frame.seq == seq).map(|(id, _)| id.clone())
    }

    pub fn frame_parent(&self, frame: &str) -> Option<String> {
        self.state.lock().expect("page lock").frames.get(frame).and_then(|f| f.parent.clone())
    }

    pub fn frame_session(&self, frame: &str) -> Option<Arc<str>> {
        self.state.lock().expect("page lock").frames.get(frame).map(|f| Arc::clone(&f.session))
    }

    /// Live page and out-of-process frame sessions of the tab, each once; `true` marks page sessions.
    pub fn sessions(&self) -> Vec<(Arc<str>, bool)> {
        let state = self.state.lock().expect("page lock");
        let mut sessions: Vec<(Arc<str>, bool)> =
            state.pages.values().map(|(session, _)| (Arc::clone(session), true)).collect();
        for frame in state.frames.values() {
            if !sessions.iter().any(|(session, _)| *session == frame.session) {
                sessions.push((Arc::clone(&frame.session), false));
            }
        }
        sessions
    }

    pub fn is_local(&self) -> bool {
        crate::console::is_local_url(&self.url())
    }

    /// Whether input on this page should look human.
    pub fn human(&self) -> bool {
        match self.humanize {
            Humanize::Always => true,
            Humanize::Never => false,
            Humanize::Auto => !self.is_local(),
        }
    }

    async fn run(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<Event>) {
        while let Some(event) = events.recv().await {
            if !self.live.load(Ordering::Acquire) {
                break;
            }
            self.on_event(&event);
        }
        self.mark_detached();
        crate::trace!("tab {}: event loop ended", self.tab);
    }

    fn on_event(self: &Arc<Self>, event: &Event) {
        match event.method.split_once('.').map_or("", |(domain, _)| domain) {
            "Target" => self.on_target_event(event),
            "Page" => self.on_page_event(event),
            "Runtime" => self.on_runtime_event(event),
            "Network" | "Fetch" => crate::network::on_event(self, event),
            "Log" => crate::console::on_event(self, event),
            "Input" if event.method == "Input.dragIntercepted" => {
                if let Ok(value) = event.parse::<Value>() {
                    self.signal(Signal::DragIntercepted(value.get("data").cloned().unwrap_or(Value::Null)));
                }
            }
            _ => crate::devtools::on_event(self, event),
        }
    }

    fn on_target_event(self: &Arc<Self>, event: &Event) {
        match event.method.as_str() {
            "Target.attachedToTarget" => {
                if let Ok(attached) = event.parse::<AttachedEvent>() {
                    self.on_attached(attached, event.session.as_deref());
                }
            }
            "Target.detachedFromTarget" => {
                if let Ok(detached) = event.parse::<DetachedEvent>() {
                    self.on_detached(&detached.session_id);
                }
            }
            _ => crate::devtools::on_event(self, event),
        }
    }

    fn on_page_event(self: &Arc<Self>, event: &Event) {
        let session = event.session.as_ref();
        match event.method.as_str() {
            "Page.frameAttached" => {
                if let (Ok(attached), Some(session)) = (event.parse::<FrameAttached>(), session) {
                    let mut state = self.state.lock().expect("page lock");
                    state.next_seq += 1;
                    let frame = Frame {
                        session: Arc::clone(session),
                        parent: attached.parent_frame_id,
                        seq: state.next_seq,
                        world: None,
                    };
                    state.frames.entry(attached.frame_id).or_insert(frame);
                }
            }
            "Page.frameNavigated" => {
                if let (Ok(navigated), Some(session)) = (event.parse::<FrameNavigated>(), session) {
                    self.on_navigated(navigated.frame, session, navigated.kind == "BackForwardCacheRestore");
                }
            }
            "Page.frameDetached" => {
                if let Ok(detached) = event.parse::<FrameDetached>()
                    && detached.reason != "swap"
                {
                    self.state.lock().expect("page lock").frames.remove(&detached.frame_id);
                }
            }
            "Page.navigatedWithinDocument" => {
                if let Ok(within) = event.parse::<NavigatedWithinDocument>() {
                    let mut state = self.state.lock().expect("page lock");
                    if state.main_frame.as_deref() == Some(within.frame_id.as_str()) {
                        state.url = within.url;
                        state.navigations += 1;
                        state.pending = None;
                    }
                    drop(state);
                    self.signal(Signal::Committed);
                }
            }
            "Page.frameStartedNavigating" | "Page.frameRequestedNavigation" => {
                // New tabs, windows and downloads leave this document where it is.
                if let Ok(starting) = event.parse::<FrameStarting>()
                    && starting.disposition.as_deref().is_none_or(|disposition| disposition == "currentTab")
                {
                    let mut state = self.state.lock().expect("page lock");
                    if state.main_frame.as_deref() == Some(starting.frame_id.as_str()) {
                        state.starts += 1;
                        state.pending = Some(starting.url);
                        state.history = starting.navigation_type.is_some_and(|kind| kind.starts_with("history"));
                        drop(state);
                        self.console.lock().expect("console lock").set_restoring(false);
                        crate::console::reconsider(self);
                        self.signal(Signal::NavigationStarted);
                    }
                }
            }
            "Page.frameStoppedLoading" => {
                // Ends a navigation that never commits (a download, a 204 response) as well. A
                // history navigation may be a restore from the back/forward cache, whose commit
                // follows: until then capture stays as the navigation set it.
                if let Ok(stopped) = event.parse::<FrameStopped>() {
                    let mut state = self.state.lock().expect("page lock");
                    if state.main_frame.as_deref() == Some(stopped.frame_id.as_str()) {
                        let ended = state.pending.take().is_some();
                        let history = state.history;
                        drop(state);
                        if ended && history {
                            self.console.lock().expect("console lock").set_restoring(true);
                        } else if ended {
                            crate::console::reconsider(self);
                        }
                        self.signal(Signal::Lifecycle);
                    }
                }
            }
            "Page.lifecycleEvent" => {
                if let (Ok(lifecycle), Some(session)) = (event.parse::<LifecycleEvent>(), session) {
                    let mut state = self.state.lock().expect("page lock");
                    // Main frames of the page and of prerendered pages; iframes don't count.
                    let page_session = state.pages.values().any(|(page, _)| page == session);
                    let main = state.frames.get(&lifecycle.frame_id).is_some_and(|frame| frame.parent.is_none());
                    if page_session && main {
                        state.record_lifecycle(&lifecycle.loader_id, &[lifecycle.name.as_str()]);
                        drop(state);
                        self.signal(Signal::Lifecycle);
                    }
                }
            }
            "Page.javascriptDialogOpening" => {
                if let (Ok(opening), Some(session)) = (event.parse::<DialogOpening>(), session) {
                    self.on_dialog(opening, session);
                }
            }
            "Page.javascriptDialogClosed" => {
                self.state.lock().expect("page lock").dialog = None;
                self.signal(Signal::Dialog);
            }
            "Page.fileChooserOpened" => {
                if let Ok(chooser) = event.parse::<FileChooserOpened>() {
                    let multiple = chooser.mode == "selectMultiple";
                    self.signal(Signal::FileChooser { backend_node_id: chooser.backend_node_id, multiple });
                }
            }
            _ => crate::devtools::on_event(self, event),
        }
    }

    fn on_runtime_event(self: &Arc<Self>, event: &Event) {
        let session = event.session.as_ref();
        match event.method.as_str() {
            "Runtime.executionContextCreated" => {
                if let (Ok(created), Some(session)) = (event.parse::<ContextCreated>(), session) {
                    let mut state = self.state.lock().expect("page lock");
                    // Only page and frame sessions: hba's console session has its own `Runtime`.
                    let ours = state.pages.values().any(|(page, _)| page == session)
                        || state.frames.values().any(|frame| frame.session == *session);
                    if !ours {
                        return;
                    }
                    let context = created.context;
                    let text = |key: &str| context.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
                    let aux = context.get("auxData");
                    state.contexts.push(ExecutionContext {
                        id: context.get("id").and_then(Value::as_i64).unwrap_or_default(),
                        session: Arc::clone(session),
                        origin: text("origin"),
                        name: text("name"),
                        kind: aux.and_then(|a| a.get("type")).and_then(Value::as_str).unwrap_or_default().to_owned(),
                        frame: aux.and_then(|a| a.get("frameId")).and_then(Value::as_str).map(str::to_owned),
                    });
                }
            }
            "Runtime.executionContextDestroyed" => {
                if let (Ok(destroyed), Some(session)) = (event.parse::<ContextDestroyed>(), session) {
                    let mut state = self.state.lock().expect("page lock");
                    state.contexts.retain(|c| !(c.id == destroyed.execution_context_id && c.session == *session));
                }
            }
            "Runtime.executionContextsCleared" => {
                if let Some(session) = session {
                    self.state.lock().expect("page lock").contexts.retain(|c| c.session != *session);
                }
            }
            _ => crate::console::on_event(self, event),
        }
    }

    fn on_attached(self: &Arc<Self>, attached: AttachedEvent, parent: Option<&str>) {
        let info = attached.target_info;
        let session: Arc<str> = Arc::from(attached.session_id.as_str());
        match info.kind.as_str() {
            // A detached page routes nothing; its sessions go with it.
            "page" if parent == Some(self.tab_session.as_str()) => {
                if !self.route(&session) {
                    return;
                }
                let prerender = info.subtype.as_deref() == Some("prerender");
                {
                    let mut state = self.state.lock().expect("page lock");
                    state.pages.insert(info.target_id.clone(), (Arc::clone(&session), prerender));
                    if !prerender {
                        state.primary = Some(Arc::clone(&session));
                        state.page_target = Some(info.target_id.clone());
                    }
                }
                tokio::spawn(Arc::clone(self).init_page(session, !prerender));
            }
            "iframe" => {
                if !self.route(&session) {
                    return;
                }
                {
                    let mut state = self.state.lock().expect("page lock");
                    state.next_seq += 1;
                    let seq = state.next_seq;
                    // An out-of-process frame's target id is its frame id.
                    let frame = state.frames.entry(info.target_id.clone()).or_insert(Frame {
                        session: Arc::clone(&session),
                        parent: None,
                        seq,
                        world: None,
                    });
                    frame.session = Arc::clone(&session);
                    frame.world = None;
                }
                tokio::spawn(Arc::clone(self).init_frame(session));
            }
            _ => {
                // Workers and other children are not ours to hold open.
                let cdp = self.cdp.clone();
                tokio::spawn(async move {
                    cdp.send("Runtime.runIfWaitingForDebugger", json!({}), Some(&session)).await.ok();
                    cdp.send("Target.detachFromTarget", json!({"sessionId": &*session}), None).await.ok();
                });
            }
        }
    }

    fn on_detached(&self, session: &str) {
        self.cdp.unroute(session);
        let mut state = self.state.lock().expect("page lock");
        state.pages.retain(|_, (s, _)| &**s != session);
        if state.primary.as_deref() == Some(session) {
            state.primary = None;
            state.page_target = None;
        }
        state.frames.retain(|_, frame| &*frame.session != session);
        state.contexts.retain(|context| &*context.session != session);
    }

    /// Chromium activates a prerendered page by swapping it into the tab. Once its target loses
    /// the `prerender` subtype it is the tab's document.
    pub fn on_target_changed(self: &Arc<Self>, info: &crate::browser::TargetInfo) {
        if info.subtype.as_deref().is_some_and(|subtype| !subtype.is_empty()) {
            return;
        }
        let session = {
            let mut state = self.state.lock().expect("page lock");
            let Some((session, prerender)) = state.pages.get_mut(&info.target_id) else { return };
            if !*prerender {
                return;
            }
            *prerender = false;
            let session = Arc::clone(session);
            state.primary = Some(Arc::clone(&session));
            state.page_target = Some(info.target_id.clone());
            session
        };
        let page = Arc::clone(self);
        tokio::spawn(async move {
            match page.cdp.call::<FrameTree>("Page.getFrameTree", json!({}), Some(&session)).await {
                Ok(tree) => {
                    page.load_tree(&tree.frame_tree, &session, true);
                    {
                        let mut state = page.state.lock().expect("page lock");
                        state.navigations += 1;
                        state.pending = None;
                        state.dialog = None;
                    }
                    page.signal(Signal::Committed);
                    // The new page session has never had console capture.
                    crate::console::on_session(&page, &session).await;
                }
                Err(error) => eprintln!("hba: adopting the activated prerendered page failed: {error:#}"),
            }
        });
    }

    async fn init_page(self: Arc<Self>, session: Arc<str>, primary: bool) {
        let outcome = self.prepare(&session, true).await;
        if let Err(error) = outcome {
            eprintln!("hba: preparing page session failed: {error:#}");
        }
        if primary {
            let _ = self.ready.send(true);
        }
    }

    async fn init_frame(self: Arc<Self>, session: Arc<str>) {
        if let Err(error) = self.prepare(&session, false).await {
            eprintln!("hba: preparing frame session failed: {error:#}");
        }
    }

    /// Enables what `hba` needs on a page or out-of-process frame session and loads its frame tree.
    async fn prepare(&self, session: &Arc<str>, is_page: bool) -> Result<()> {
        let cdp = &self.cdp;
        let s = Some(&**session);
        let page_enable = cdp.send("Page.enable", json!({}), s);
        let network = cdp.send("Network.enable", json!({"maxPostDataSize": 65_536}), s);
        let attach = cdp.send(
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true,
                   "filter": [{"type": "iframe"}, {"exclude": true}]}),
            s,
        );
        let tree = cdp.call::<FrameTree>("Page.getFrameTree", json!({}), s);
        // Background tabs otherwise stop rendering: no rAF, throttled timers, hanging screenshots.
        let focus = async {
            if is_page {
                cdp.send("Emulation.setFocusEmulationEnabled", json!({"enabled": true}), s).await
            } else {
                Ok(())
            }
        };
        // A new frame or prerendered page waits for the debugger, so interception and emulation
        // are in place before its first request and script. Every command goes out, in this
        // order, before any reply is awaited: a waiting page may answer only once it runs.
        let (enabled, network, attach, tree, focus, (), (), resumed) = tokio::join!(
            biased;
            page_enable,
            network,
            attach,
            tree,
            focus,
            crate::routes::on_session(self, session),
            crate::emulate::on_session(self, session, is_page),
            cdp.send("Runtime.runIfWaitingForDebugger", json!({}), s),
        );
        enabled.and(network).and(attach).and(focus).and(resumed)?;
        self.load_tree(&tree?.frame_tree, session, is_page);
        // Enabling lifecycle events replays the ones already fired, so the frames must be known.
        cdp.send("Page.setLifecycleEventsEnabled", json!({"enabled": true}), s).await?;
        crate::console::on_session(self, session).await;
        Ok(())
    }

    fn load_tree(&self, root: &FrameNode, session: &Arc<str>, is_page: bool) {
        let mut state = self.state.lock().expect("page lock");
        if is_page && state.primary.as_ref() == Some(session) {
            state.main_frame = Some(root.frame.id.clone());
            state.url.clone_from(&root.frame.url);
            state.loader.clone_from(&root.frame.loader_id);
            self.console.lock().expect("console lock").set_document(root.frame.loader_id.clone());
        }
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let seq = if node.frame.parent_id.is_none() && is_page {
                0
            } else {
                state.next_seq += 1;
                state.next_seq
            };
            let entry = state.frames.entry(node.frame.id.clone()).or_insert(Frame {
                session: Arc::clone(session),
                parent: node.frame.parent_id.clone(),
                seq,
                world: None,
            });
            entry.session = Arc::clone(session);
            if entry.parent.is_none() {
                entry.parent.clone_from(&node.frame.parent_id);
            }
            stack.extend(&node.child_frames);
        }
    }

    fn on_navigated(&self, frame: FrameData, session: &Arc<str>, restored: bool) {
        let mut state = self.state.lock().expect("page lock");
        let is_main = frame.parent_id.is_none() && state.primary.as_ref() == Some(session);
        if is_main {
            state.main_frame = Some(frame.id.clone());
            state.url.clone_from(&frame.url);
            state.loader.clone_from(&frame.loader_id);
            self.console.lock().expect("console lock").set_document(frame.loader_id.clone());
            state.navigations += 1;
            state.pending = None;
            state.dialog = None;
            // A document restored from the back/forward cache does not load again.
            if restored && let Some(loader) = &frame.loader_id {
                state.record_lifecycle(loader, &["DOMContentLoaded", "load", "networkIdle"]);
            }
        }
        let next = state.next_seq + 1;
        let entry = state.frames.entry(frame.id).or_insert(Frame {
            session: Arc::clone(session),
            parent: frame.parent_id.clone(),
            seq: next,
            world: None,
        });
        let fresh = entry.seq == next;
        entry.world = None;
        if is_main {
            entry.seq = 0;
        }
        if fresh && !is_main {
            state.next_seq = next;
        }
        drop(state);
        if is_main {
            crate::console::reconsider(self);
        }
        self.signal(Signal::Committed);
    }

    fn on_dialog(&self, opening: DialogOpening, session: &Arc<str>) {
        let DialogOpening { kind, message, default_prompt } = opening;
        // Alerts and leave-page prompts only inform; confirm and prompt wait for a decision.
        if kind == "alert" || kind == "beforeunload" {
            self.state.lock().expect("page lock").notes.push(format!("{kind} {message:?} (accepted)"));
            let (cdp, session) = (self.cdp.clone(), Arc::clone(session));
            tokio::spawn(async move {
                cdp.send("Page.handleJavaScriptDialog", json!({"accept": true}), Some(&session)).await.ok();
            });
        } else {
            self.state.lock().expect("page lock").dialog = Some(Dialog { kind, message, default_prompt });
        }
        self.signal(Signal::Dialog);
    }

    /// Sends an input event and waits for the page to take it. A dialog the event opens blocks
    /// the page, so it ends the wait: the event was delivered, and the dialog is reported for
    /// the agent to handle.
    pub async fn input(&self, method: &str, params: Value) -> Result<()> {
        self.send_input(method, &params)?.await
    }

    /// Sends an input event now and returns the wait for the page to take it, so events can go
    /// out back to back: a window that is not on screen takes a lone pointer move only after a
    /// second, but at once when another event follows.
    pub fn send_input(&self, method: &str, params: &Value) -> Result<impl Future<Output = Result<()>> + use<'_>> {
        if let Some(dialog) = self.dialog() {
            return Err(Self::dialog_error(&dialog));
        }
        let session = self.session()?;
        let mut signals = self.subscribe();
        let call = self.cdp.start(method, params, Some(&session), crate::cdp::CALL_TIMEOUT)?;
        Ok(async move {
            tokio::pin!(call);
            loop {
                tokio::select! {
                    result = &mut call => return result.map(drop),
                    signal = signals.recv() => match signal {
                        Ok(Signal::Dialog) | Err(broadcast::error::RecvError::Lagged(_)) if self.dialog().is_some() => return Ok(()),
                        Ok(Signal::Detached) | Err(broadcast::error::RecvError::Closed) => bail!("the tab was closed"),
                        _ => {}
                    },
                }
            }
        })
    }

    pub async fn handle_dialog(&self, accept: bool, text: Option<&str>) -> Result<Dialog> {
        let dialog = self.dialog().context("no dialog is open")?;
        let session = self.session()?;
        let mut params = json!({"accept": accept});
        if let Some(text) = text {
            params["promptText"] = json!(text);
        }
        self.cdp.send("Page.handleJavaScriptDialog", params, Some(&session)).await?;
        self.state.lock().expect("page lock").dialog = None;
        Ok(dialog)
    }

    pub fn dialog_error(dialog: &Dialog) -> anyhow::Error {
        anyhow!(
            "a {} dialog is open: {:?}; run `hba dialog accept` or `hba dialog dismiss`",
            dialog.kind,
            dialog.message
        )
    }

    /// Runs a future of protocol calls that need the page's renderer, failing fast if a dialog
    /// blocks the page: the renderer answers nothing until the dialog is closed.
    pub(crate) async fn guarded<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        if let Some(dialog) = self.dialog() {
            return Err(Self::dialog_error(&dialog));
        }
        let mut signals = self.subscribe();
        tokio::pin!(future);
        loop {
            tokio::select! {
                result = &mut future => return result,
                signal = signals.recv() => match signal {
                    Ok(Signal::Dialog) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(dialog) = self.dialog() {
                            return Err(Self::dialog_error(&dialog));
                        }
                    }
                    Ok(Signal::Detached) | Err(broadcast::error::RecvError::Closed) => bail!("the tab was closed"),
                    Ok(_) => {}
                },
            }
        }
    }

    /// A protocol call answered by the page's renderer (`DOM.*`, most `Page.*`) on `session`,
    /// failing fast while a dialog is open.
    pub async fn renderer_call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
        session: &str,
    ) -> Result<T> {
        self.guarded(self.cdp.call(method, params, Some(session))).await
    }

    /// Forgets the execution contexts recorded for `session`, after `Runtime.disable`: disabling
    /// reports nothing, and enabling again reports every context anew.
    pub fn forget_contexts(&self, session: &str) {
        self.state.lock().expect("page lock").contexts.retain(|context| &*context.session != session);
    }

    /// `hba`'s world in `frame`, created on first use in each document.
    pub async fn world(&self, frame: &str) -> Result<World> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Created {
            execution_context_id: i64,
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Evaluate<'a> {
            expression: crate::injected::Bootstrap<'a>,
            context_id: i64,
        }
        let (session, seq) = {
            let state = self.state.lock().expect("page lock");
            let entry = state.frames.get(frame).with_context(|| format!("frame {frame} is gone"))?;
            if let Some(world) = &entry.world {
                return Ok(world.clone());
            }
            (Arc::clone(&entry.session), entry.seq)
        };
        // A navigation can replace the document between the two calls.
        let created: Created = self
            .renderer_call(
                "Page.createIsolatedWorld",
                json!({"frameId": frame, "worldName": self.source.world_name, "grantUniversalAccess": false}),
                &session,
            )
            .await
            .map_err(navigated)?;
        let params = Evaluate { expression: self.source.bootstrap(seq), context_id: created.execution_context_id };
        let evaluated: Evaluated = self.renderer_call("Runtime.evaluate", params, &session).await.map_err(navigated)?;
        if let Some(details) = evaluated.exception_details {
            bail!("starting the page engine failed: {}", details.message());
        }
        let engine = evaluated.result.object_id.context("the page engine did not start")?;
        let world = World { session, frame: frame.to_owned(), context: created.execution_context_id, engine };
        if let Some(entry) = self.state.lock().expect("page lock").frames.get_mut(frame) {
            entry.world = Some(world.clone());
        }
        Ok(world)
    }

    pub async fn main_world(&self) -> Result<World> {
        let frame = self.main_frame()?;
        self.world(&frame).await
    }

    fn forget_world(&self, world: &World) {
        if let Some(entry) = self.state.lock().expect("page lock").frames.get_mut(&world.frame)
            && entry.world.as_ref().is_some_and(|w| w.context == world.context)
        {
            entry.world = None;
        }
    }

    /// Calls `function` with `this` = the Playwright engine and returns its JSON result.
    pub async fn call(&self, world: &World, function: &str, args: &[Arg<'_>]) -> Result<Value> {
        let object = self.call_raw(world, function, args, true).await?;
        Ok(object.value.unwrap_or(Value::Null))
    }

    /// Like [`Page::call`] but returns a remote object id (`None` for null/undefined).
    pub async fn call_handle(&self, world: &World, function: &str, args: &[Arg<'_>]) -> Result<Option<String>> {
        let object = self.call_raw(world, function, args, false).await?;
        Ok(object.object_id.filter(|_| object.subtype.as_deref() != Some("null")))
    }

    async fn call_raw(&self, world: &World, function: &str, args: &[Arg<'_>], by_value: bool) -> Result<RemoteObject> {
        let arguments: Vec<Value> = args
            .iter()
            .map(|arg| match arg {
                Arg::Value(value) => json!({"value": value}),
                Arg::Object(id) => json!({"objectId": id}),
            })
            .collect();
        let params = json!({
            "objectId": world.engine,
            "functionDeclaration": function,
            "arguments": arguments,
            "returnByValue": by_value,
            "awaitPromise": true,
            "objectGroup": "hba",
        });
        let result: Result<Evaluated> = self.renderer_call("Runtime.callFunctionOn", &params, &world.session).await;
        let evaluated = result.map_err(|error| {
            if is_stale(&error) {
                self.forget_world(world);
            }
            navigated(error)
        })?;
        if let Some(details) = evaluated.exception_details {
            bail!("{}", details.message());
        }
        Ok(evaluated.result)
    }

    /// Evaluates page JavaScript in the main world and returns its value, as [`evaluate_on`] does,
    /// failing fast when a dialog blocks the page; a navigation meanwhile is a retryable error.
    pub async fn evaluate(&self, expression: &str, user_gesture: bool) -> Result<Value> {
        let session = self.session()?;
        let timeout = crate::cdp::CALL_TIMEOUT;
        self.guarded(evaluate_on(&self.cdp, &session, None, expression, user_gesture, timeout)).await.map_err(navigated)
    }

    /// Evaluates JavaScript in a frame's main world, reached through the frame's document node.
    pub async fn evaluate_in_frame(&self, frame: &str, expression: &str) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Described {
            node: Node,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            backend_node_id: i64,
        }
        #[derive(Deserialize)]
        struct Resolved {
            object: RemoteObject,
        }
        let world = self.world(frame).await?;
        let document = self
            .call_handle(&world, "function() { return this.document; }", &[])
            .await?
            .context("the frame has no document")?;
        let described: Described =
            self.renderer_call("DOM.describeNode", json!({"objectId": document}), &world.session).await?;
        let resolved: Resolved = self
            .renderer_call(
                "DOM.resolveNode",
                json!({"backendNodeId": described.node.backend_node_id, "objectGroup": "hba"}),
                &world.session,
            )
            .await?;
        let object = resolved.object.object_id.context("the frame document is gone")?;
        let evaluated: Evaluated = self
            .renderer_call(
                "Runtime.callFunctionOn",
                json!({"objectId": object, "functionDeclaration": "function(code) { return (0, eval)(code); }",
                       "arguments": [{"value": expression}], "returnByValue": true, "awaitPromise": true}),
                &world.session,
            )
            .await?;
        if let Some(details) = evaluated.exception_details {
            bail!("{}", details.message());
        }
        Ok(remote_value(evaluated.result))
    }

    /// Releases temporary remote objects held for the last command, without waiting for the
    /// replies: a busy renderer must not hold up the command's answer. A page showing a dialog
    /// answers nothing until it closes; its objects are released after a later command.
    pub fn release(&self) {
        if self.dialog().is_some() {
            return;
        }
        let sessions: HashSet<Arc<str>> = {
            let state = self.state.lock().expect("page lock");
            let worlds = state.frames.values().filter(|f| f.world.is_some()).map(|f| Arc::clone(&f.session));
            worlds.chain(state.primary.clone()).collect()
        };
        let params = json!({"objectGroup": "hba"});
        for session in &sessions {
            // The command is written now; dropping the reply's future only stops waiting for it.
            drop(self.cdp.start("Runtime.releaseObjectGroup", &params, Some(session), crate::cdp::CALL_TIMEOUT));
        }
    }

    /// Resolves a ref (`e5`, `f1e3`) or Playwright selector, waiting up to `timeout` for a selector to match.
    pub async fn resolve(&self, target: &str, timeout: Duration) -> Result<Element> {
        if let Some((seq, reference)) = parse_ref(target) {
            let frame = self.frame_by_seq(seq).with_context(|| {
                format!("ref {reference} is not on the page any more (frame f{seq} is gone); run `hba snap`")
            })?;
            let world = self.world(&frame).await?;
            let current = self.state.lock().expect("page lock").snapshotted.get(&frame) == Some(&world.context);
            if !current {
                bail!("ref {reference} is from an earlier page; run `hba snap` for fresh refs");
            }
            let object = self
                .call_handle(
                    &world,
                    "function(ref) { return this.querySelector(this.parseSelector('aria-ref=' + ref), this.document, false) || null; }",
                    &[Arg::Value(json!(reference))],
                )
                .await?
                .with_context(|| format!("ref {reference} is not on the page any more; run `hba snap` for fresh refs"))?;
            let label = self.describe(&world, &object).await.unwrap_or_default();
            return Ok(Element { world, object, label });
        }
        let deadline = Instant::now() + timeout;
        loop {
            let world = self.main_world().await?;
            let object = self
                .call_handle(
                    &world,
                    "function(selector) { \
                       const parsed = this.parseSelector(selector); \
                       const all = this.querySelectorAll(parsed, this.document); \
                       if (all.length > 1) throw new Error(`${selector} matched ${all.length} elements: ` + \
                         all.slice(0, 3).map(e => this.previewNode(e)).join(', ') + \
                         (all.length > 3 ? ', …' : '') + '; use a ref from `hba snap` or a narrower selector'); \
                       return all[0] || null; }",
                    &[Arg::Value(json!(target))],
                )
                .await?;
            if let Some(object) = object {
                let label = self.describe(&world, &object).await.unwrap_or_default();
                return Ok(Element { world, object, label });
            }
            if Instant::now() >= deadline {
                bail!("no element matches {target:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `role "name"` of an element, for messages.
    async fn describe(&self, world: &World, object: &str) -> Result<String> {
        let value = self
            .call(
                world,
                "function(e) { \
                   const role = this.utils.getAriaRole(e); \
                   const name = this.utils.normalizeWhiteSpace(this.utils.getElementAccessibleNameText(e, false) || ''); \
                   if (!role) return this.previewNode(e); \
                   return name ? `${role} ${JSON.stringify(name.length > 60 ? name.slice(0, 59) + '…' : name)}` : role; }",
                &[Arg::Object(object)],
            )
            .await?;
        Ok(value.as_str().unwrap_or_default().chars().take(100).collect())
    }

    /// Navigates the main frame and waits for `wait`.
    pub async fn goto(&self, url: &str, wait: LoadState, timeout: Duration) -> Result<()> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Navigated {
            #[serde(default)]
            loader_id: Option<String>,
            #[serde(default)]
            error_text: Option<String>,
            /// The response was a download (an attachment), not a document.
            #[serde(default)]
            is_download: bool,
        }
        let session = self.session()?;
        let mut signals = self.subscribe();
        let navigated: Navigated =
            self.heading_to(url, self.renderer_call("Page.navigate", json!({"url": url}), &session)).await?;
        if navigated.is_download {
            return Ok(());
        }
        if let Some(error) = navigated.error_text.filter(|e| !e.is_empty()) {
            bail!("navigation to {url} failed: {error}");
        }
        let Some(loader) = navigated.loader_id else {
            return Ok(()); // Same-document navigation.
        };
        self.wait_for_loader(&loader, &mut signals, timeout).await?;
        self.wait_load(wait, timeout).await
    }

    /// Starts a main-frame navigation to `url` with the protocol call `start`. Console capture
    /// follows the destination first, before its document can run any script. When nothing
    /// navigates (the call fails, or the command is cancelled before it), the navigation is no
    /// longer pending and capture follows the current document again.
    pub async fn heading_to<T>(&self, url: &str, start: impl Future<Output = Result<T>>) -> Result<T> {
        let starts = {
            let mut state = self.state.lock().expect("page lock");
            state.pending = Some(url.to_owned());
            state.starts
        };
        let stay = Undo::new({
            let (page, url) = (self.arc(), url.to_owned());
            async move { page.stayed(&url, starts) }
        });
        let started = async {
            crate::console::apply(self).await?;
            start.await
        }
        .await;
        if started.is_ok() {
            stay.disarm();
        } else {
            stay.finish().await;
        }
        started
    }

    /// Forgets the pending navigation to `url` that [`Page::heading_to`] set when `starts`
    /// navigations had started, unless one started since.
    fn stayed(&self, url: &str, starts: u64) {
        let mut state = self.state.lock().expect("page lock");
        if state.starts != starts || state.pending.as_deref() != Some(url) {
            return;
        }
        state.pending = None;
        drop(state);
        crate::console::reconsider(self);
    }

    async fn wait_for_loader(
        &self,
        loader: &str,
        signals: &mut broadcast::Receiver<Signal>,
        timeout: Duration,
    ) -> Result<()> {
        self.wait_until(signals, timeout, || self.loader().as_deref() == Some(loader)).await
    }

    /// Waits for a load state of the current document.
    pub async fn wait_load(&self, wait: LoadState, timeout: Duration) -> Result<()> {
        let mut signals = self.subscribe();
        let fired = |name: &str| self.state.lock().expect("page lock").fired(name);
        match wait {
            LoadState::None | LoadState::Commit => Ok(()),
            LoadState::Load => self.wait_until(&mut signals, timeout, || fired("load")).await,
            LoadState::Idle => {
                self.wait_until(&mut signals, timeout, || fired("load")).await?;
                self.wait_until(&mut signals, timeout, || fired("networkIdle")).await
            }
            LoadState::Dom => {
                self.wait_until(&mut signals, timeout, || fired("DOMContentLoaded")).await?;
                // Give scripts a moment to render without waiting on slow third parties.
                let _ = self.wait_until(&mut signals, Duration::from_millis(1500).min(timeout), || fired("load")).await;
                self.quiet(Instant::now(), Duration::from_secs(1)).await;
                Ok(())
            }
        }
    }

    /// Polls `done` on every page signal until it holds or `timeout` passes.
    pub async fn wait_until(
        &self,
        signals: &mut broadcast::Receiver<Signal>,
        timeout: Duration,
        mut done: impl FnMut() -> bool,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if done() {
                return Ok(());
            }
            if let Some(dialog) = self.dialog() {
                return Err(Self::dialog_error(&dialog));
            }
            match tokio::time::timeout_at(deadline, signals.recv()).await {
                Err(_) => bail!("timed out after {:.1}s", timeout.as_secs_f32()),
                Ok(Ok(Signal::Detached) | Err(broadcast::error::RecvError::Closed)) => bail!("the tab was closed"),
                Ok(_) => {}
            }
        }
    }

    /// Waits until the DOM has not changed for [`QUIET`] and no document, fetch or XHR started
    /// after `since` is in flight, for at most `cap`. Earlier requests (long polls, beacons) are
    /// ignored. Returns early when a navigation or dialog starts.
    pub async fn quiet(&self, since: Instant, cap: Duration) {
        let deadline = Instant::now() + cap;
        let starts = self.state.lock().expect("page lock").starts;
        let mut last = None;
        let mut stable_from = Instant::now();
        while Instant::now() < deadline {
            let count = match self.main_world().await {
                Ok(world) => self.call(&world, "function() { return this.hbaMutations; }", &[]).await.ok(),
                Err(_) => None,
            };
            if self.dialog().is_some() || self.state.lock().expect("page lock").starts != starts {
                return;
            }
            let busy = self.network.lock().expect("network lock").busy_since(since);
            crate::trace!("quiet: mutations {count:?} busy {busy} stable {:?}", stable_from.elapsed());
            if count != last || busy {
                last = count;
                stable_from = Instant::now();
            } else if stable_from.elapsed() >= QUIET {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub fn mark(&self) -> Mark {
        let state = self.state.lock().expect("page lock");
        Mark {
            navigations: state.navigations,
            starts: state.starts,
            url: state.url.clone(),
            console_seq: self.console.lock().expect("console lock").seq(),
            network_seq: self.network.lock().expect("network lock").seq(),
            download_seq: self.downloads.lock().expect("downloads lock").seq(),
            at: Instant::now(),
        }
    }

    /// After an input action: waits for the navigation it started, or for the DOM to settle.
    pub async fn settle(&self, mark: &Mark, timeout: Duration) -> Result<()> {
        self.quiet(mark.at, SETTLE_CAP).await;
        let navigating = {
            let state = self.state.lock().expect("page lock");
            state.starts != mark.starts || state.navigations != mark.navigations
        };
        if !navigating || self.dialog().is_some() || !self.is_live() {
            return Ok(());
        }
        // A started navigation may still be waiting for the network before it commits, or end
        // without committing: a download or a 204 response stops the frame loading instead.
        let downloaded =
            || self.downloads.lock().expect("downloads lock").items.last().is_some_and(|item| item.started >= mark.at);
        let ended = || {
            let state = self.state.lock().expect("page lock");
            state.navigations != mark.navigations || state.pending.is_none()
        };
        let mut signals = self.subscribe();
        // A dialog ends the wait too (as an error); the report names it.
        let waited = self.wait_until(&mut signals, timeout, || ended() || downloaded()).await;
        if waited.is_err() || self.dialog().is_some() {
            return Ok(());
        }
        if self.navigations() != mark.navigations {
            self.wait_load(LoadState::Dom, timeout).await.ok();
        } else if !downloaded() && self.network.lock().expect("network lock").turned_into_download(mark.at) {
            // The browser announces the download just after the navigation ends; report it.
            self.wait_until(&mut signals, timeout.min(Duration::from_secs(1)), downloaded).await.ok();
        }
        Ok(())
    }

    /// Appends what changed since `mark` (navigation, dialog, errors, downloads).
    pub fn report(&self, mark: &Mark, out: &mut String) {
        use std::fmt::Write as _;
        let (url, navigations) = {
            let state = self.state.lock().expect("page lock");
            (state.url.clone(), state.navigations)
        };
        if url != mark.url {
            let _ = writeln!(out, "→ {}", crate::output::short_url(&url, 120));
        } else if navigations != mark.navigations {
            let _ = writeln!(out, "↻ reloaded");
        }
        for note in self.take_notes() {
            let _ = writeln!(out, "· {note}");
        }
        if let Some(dialog) = self.dialog() {
            let _ = writeln!(out, "! {} dialog: {:?} — `hba dialog accept|dismiss`", dialog.kind, dialog.message);
        }
        self.console.lock().expect("console lock").report_since(mark.console_seq, out);
        self.network.lock().expect("network lock").report_since(mark.network_seq, out);
        self.downloads.lock().expect("downloads lock").report_since(mark.download_seq, out);
    }

    /// Reads this tab's profile directory from `chrome://version` (used once per profile).
    pub async fn profile_directory(&self) -> Result<String> {
        self.goto("chrome://version/", LoadState::Load, Duration::from_secs(10)).await?;
        let path = self.evaluate("document.getElementById('profile_path')?.textContent ?? ''", false).await?;
        let path = path.as_str().unwrap_or_default().trim().to_owned();
        let dir = std::path::Path::new(&path).file_name().and_then(|name| name.to_str()).map(str::to_owned);
        dir.with_context(|| format!("chrome://version shows no profile path ({path:?})"))
    }
}

/// Parses `e12`, `@e12` or `f2e5` into (frame sequence, ref).
pub fn parse_ref(target: &str) -> Option<(u32, String)> {
    let reference = target.strip_prefix('@').unwrap_or(target);
    let reference = reference.strip_prefix("ref=").unwrap_or(reference);
    let (seq, rest) = match reference.strip_prefix('f') {
        Some(rest) => {
            let digits = rest.find('e')?;
            (rest[..digits].parse().ok()?, &rest[digits + 1..])
        }
        None => (0, reference.strip_prefix('e')?),
    };
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())).then(|| (seq, reference.to_owned()))
}

fn is_stale(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("Cannot find context with specified id")
        || text.contains("Could not find object with given id")
        || text.contains("Execution context was destroyed")
        || text.contains("Inspected target navigated or closed")
}

/// A call that lost its document to a navigation fails with the retryable error
/// [`crate::inspect::retryable`] recognizes; other errors stay as they are.
fn navigated(error: anyhow::Error) -> anyhow::Error {
    if is_stale(&error) { anyhow!("the page navigated while hba was working on it; retry") } else { error }
}

/// The JSON value of a `returnByValue` result, with JS-only values rendered as strings.
fn remote_value(object: RemoteObject) -> Value {
    if let Some(value) = object.unserializable_value {
        return Value::String(value);
    }
    match (object.kind.as_str(), object.value) {
        ("undefined", _) => Value::Null,
        (_, Some(value)) => value,
        (_, None) => object.description.map_or(Value::Null, Value::String),
    }
}

/// Evaluates `code` on `session` (in execution context `context`, else the session's main
/// world) like the browser console: statements and top-level `await` work, a resulting promise
/// is awaited, and the value comes back as JSON; DOM nodes, functions and objects JSON cannot
/// hold come back as their description. Each protocol call may take up to `timeout`.
pub async fn evaluate_on(
    cdp: &Cdp,
    session: &str,
    context: Option<i64>,
    code: &str,
    user_gesture: bool,
    timeout: Duration,
) -> Result<Value> {
    let mut params = json!({"expression": code, "returnByValue": false, "awaitPromise": true, "replMode": true,
                            "userGesture": user_gesture, "allowUnsafeEvalBlockedByCSP": true, "objectGroup": "hba"});
    if let Some(context) = context {
        params["contextId"] = json!(context);
    }
    let evaluated = runtime_call(cdp, "Runtime.evaluate", &params, session, timeout).await?;
    if let Some(details) = evaluated.exception_details {
        bail!("{}", details.message());
    }
    value_of(cdp, session, evaluated.result, timeout).await
}

async fn runtime_call(cdp: &Cdp, method: &str, params: &Value, session: &str, timeout: Duration) -> Result<Evaluated> {
    let raw = cdp.raw_within(method, params, Some(session), timeout).await?;
    serde_json::from_str(raw.get()).with_context(|| format!("decoding the {method} result"))
}

/// Turns a remote result into JSON, awaiting promises.
async fn value_of(cdp: &Cdp, session: &str, object: RemoteObject, timeout: Duration) -> Result<Value> {
    let Some(id) = object.object_id.clone() else { return Ok(remote_value(object)) };
    match object.subtype.as_deref() {
        Some("promise") => {
            let params = json!({"promiseObjectId": id});
            let awaited = runtime_call(cdp, "Runtime.awaitPromise", &params, session, timeout).await?;
            if let Some(details) = awaited.exception_details {
                bail!("{}", details.message());
            }
            Box::pin(value_of(cdp, session, awaited.result, timeout)).await
        }
        None | Some("array") if object.kind == "object" => {
            let params =
                json!({"objectId": id, "functionDeclaration": "function() { return this; }", "returnByValue": true});
            match runtime_call(cdp, "Runtime.callFunctionOn", &params, session, timeout).await {
                Ok(evaluated) if evaluated.exception_details.is_none() => Ok(remote_value(evaluated.result)),
                _ => Ok(object.description.map_or(Value::Null, Value::String)),
            }
        }
        _ => Ok(object.description.map_or(Value::Null, Value::String)),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    /// Every command the fake browser received, as sent.
    pub(crate) type Received = Arc<Mutex<Vec<Value>>>;

    /// The browser end of a DevTools connection, enough to attach to a tab: it answers every
    /// command, reports a page once the tab auto-attaches and an out-of-process frame on
    /// `Test.addFrame`, and records every command. Attaching to the page target gives sessions
    /// `console-1`, `console-2`, … Tests steer it with `Test.emit` (send the params as an
    /// event), `Test.fail`/`Test.pass` (`method` answers with error `message` until passed) and
    /// `Test.hold`/`Test.release` (`method`'s replies wait until released).
    pub(crate) async fn fake_browser() -> (String, Received) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}", listener.local_addr().expect("address"));
        let received = Received::default();
        let log = Arc::clone(&received);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut socket = tokio_tungstenite::accept_async(stream).await.expect("handshake");
            let mut failing: HashMap<String, String> = HashMap::new();
            let mut held: HashMap<String, Vec<Value>> = HashMap::new();
            let mut consoles = 0;
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let command: Value = serde_json::from_str(text.as_str()).expect("command");
                log.lock().expect("log lock").push(command.clone());
                let method = command["method"].as_str().unwrap_or_default();
                let session = command["sessionId"].as_str().unwrap_or_default();
                let params = &command["params"];
                let named = params["method"].as_str().unwrap_or_default().to_owned();
                let mut messages = Vec::new();
                let result = match (method, session) {
                    ("Target.attachToTarget", _) if params["targetId"] == "page" => {
                        consoles += 1;
                        json!({"sessionId": format!("console-{consoles}")})
                    }
                    ("Target.attachToTarget", _) => json!({"sessionId": "tab"}),
                    ("Page.getFrameTree", "page") => {
                        json!({"frameTree": {"frame": {"id": "main", "url": "about:blank"}}})
                    }
                    ("Page.createIsolatedWorld", _) => json!({"executionContextId": 7}),
                    ("Page.getFrameTree", _) => {
                        json!({"frameTree": {"frame": {"id": "child", "parentId": "main", "url": "about:blank"}}})
                    }
                    ("Test.emit", _) => {
                        messages.push(params.clone());
                        json!({})
                    }
                    ("Test.fail", _) => {
                        failing.insert(named, params["message"].as_str().unwrap_or_default().to_owned());
                        json!({})
                    }
                    ("Test.pass", _) => {
                        failing.remove(&named);
                        json!({})
                    }
                    ("Test.hold", _) => {
                        held.insert(named, Vec::new());
                        json!({})
                    }
                    ("Test.release", _) => {
                        messages.extend(held.remove(&named).unwrap_or_default());
                        json!({})
                    }
                    _ => json!({}),
                };
                let mut reply = match failing.get(method) {
                    Some(message) => json!({"id": command["id"], "error": {"code": -32000, "message": message}}),
                    None => json!({"id": command["id"], "result": result}),
                };
                if !session.is_empty() {
                    reply["sessionId"] = json!(session);
                }
                let mut answer = vec![reply];
                let child = match (method, session) {
                    ("Target.setAutoAttach", "tab") => Some(("tab", "page", "page")),
                    ("Test.addFrame", _) => Some(("page", "frame", "iframe")),
                    _ => None,
                };
                if let Some((parent, id, kind)) = child {
                    answer.push(json!({"method": "Target.attachedToTarget", "sessionId": parent,
                        "params": {"sessionId": id, "targetInfo": {"targetId": id, "type": kind}, "waitingForDebugger": true}}));
                }
                match held.get_mut(method) {
                    Some(waiting) => waiting.extend(answer),
                    // What a test asked for arrives before the reply that confirms the request.
                    None => messages.extend(answer),
                }
                for message in messages {
                    socket.send(Message::text(message.to_string())).await.expect("send");
                }
            }
        });
        (url, received)
    }

    /// Session (`sessionId`, or the `sessionId` parameter for browser-level commands) of every
    /// `method` command received so far.
    pub(crate) fn sessions_of(received: &Received, method: &str) -> Vec<String> {
        let received = received.lock().expect("log lock");
        let session = |command: &Value| {
            let session = command["sessionId"].as_str().or_else(|| command["params"]["sessionId"].as_str());
            session.unwrap_or_default().to_owned()
        };
        received.iter().filter(|command| command["method"] == method).map(session).collect()
    }

    /// Polls `done` for up to five seconds.
    pub(crate) async fn eventually(mut done: impl FnMut() -> bool) -> bool {
        let poll = async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), poll).await.is_ok()
    }

    /// A page attached to tab `T` of a [`fake_browser`], its connection and what the browser received.
    pub(crate) async fn attached_page() -> (Arc<Page>, Cdp, Received) {
        let (url, received) = fake_browser().await;
        let (cdp, _browser_events) = Cdp::connect(&url).await.expect("connect");
        let page = Page::attach(cdp.clone(), Arc::new(Source::new()), Humanize::Never, "T").await.expect("attach");
        (page, cdp, received)
    }

    /// Steers the fake browser.
    pub(crate) async fn steer(cdp: &Cdp, command: &str, params: Value) {
        cdp.send(command, params, None).await.expect("the fake browser takes test commands");
    }

    /// Sets the console capture policy and waits for the browser, as `hba console --on|--off` does.
    async fn capture(page: &Page, capture: crate::console::Capture) -> Result<()> {
        page.console.lock().expect("console lock").capture = capture;
        crate::console::apply(page).await
    }

    fn console_session(page: &Page) -> Option<String> {
        crate::console::session(page).map(|session| session.to_string())
    }

    #[tokio::test]
    async fn new_frames_run_after_setup_and_closed_pages_are_freed() {
        let (page, cdp, received) = attached_page().await;
        let rule = crate::routes::Rule { pattern: "*.png".into(), action: crate::routes::Action::Block };
        page.routes.lock().expect("routes lock").add(rule);
        cdp.send("Test.addFrame", json!({}), None).await.expect("frame announced");
        let frame_methods = || -> Vec<String> {
            let received = received.lock().expect("log lock");
            let on_frame = received.iter().filter(|command| command["sessionId"] == "frame");
            on_frame.map(|command| command["method"].as_str().unwrap_or_default().to_owned()).collect()
        };
        assert!(eventually(|| frame_methods().iter().any(|m| m == "Page.setLifecycleEventsEnabled")).await);
        let methods = frame_methods();
        let position =
            |name: &str| methods.iter().position(|m| m == name).unwrap_or_else(|| panic!("{name}: {methods:?}"));
        let resume = position("Runtime.runIfWaitingForDebugger");
        for setup in ["Network.enable", "Target.setAutoAttach", "Fetch.enable"] {
            assert!(position(setup) < resume, "{setup} after the frame resumed: {methods:?}");
        }

        // A closed tab: its event loop ends once its sessions are unrouted, freeing the page.
        let weak = Arc::downgrade(&page);
        page.mark_detached();
        drop(page);
        assert!(eventually(|| weak.strong_count() == 0).await, "the page is still referenced");
    }

    #[tokio::test]
    async fn an_attach_cancelled_before_the_page_is_ready_lets_go_of_the_tab() {
        let (url, received) = fake_browser().await;
        let (cdp, _browser_events) = Cdp::connect(&url).await.expect("connect");
        steer(&cdp, "Test.hold", json!({"method": "Target.setAutoAttach"})).await;
        let attach = Page::attach(cdp.clone(), Arc::new(Source::new()), Humanize::Never, "T");
        assert!(tokio::time::timeout(Duration::from_millis(100), attach).await.is_err(), "the page never got ready");
        assert!(eventually(|| sessions_of(&received, "Target.detachFromTarget").contains(&"tab".to_owned())).await);
    }

    #[tokio::test]
    async fn console_sessions_follow_the_latest_decision() {
        use crate::console::Capture;
        let (page, cdp, received) = attached_page().await;
        let enabled = || sessions_of(&received, "Runtime.enable");
        let detached = || sessions_of(&received, "Target.detachFromTarget");

        // off → on → off → on: each session gets Runtime and is detached when capture goes off.
        capture(&page, Capture::On).await.expect("capture on");
        assert_eq!(console_session(&page).as_deref(), Some("console-1"));
        capture(&page, Capture::Off).await.expect("capture off");
        assert_eq!(console_session(&page), None);
        capture(&page, Capture::On).await.expect("capture on again");
        assert_eq!(console_session(&page).as_deref(), Some("console-2"));
        assert_eq!(enabled(), ["console-1", "console-2"]);
        assert_eq!(detached(), ["console-1"]);
        capture(&page, Capture::Off).await.expect("capture off again");

        // An attach that completes after capture went off again is detached without Runtime.
        steer(&cdp, "Test.hold", json!({"method": "Target.attachToTarget"})).await;
        page.console.lock().expect("console lock").capture = Capture::On;
        crate::console::reconsider(&page);
        page.console.lock().expect("console lock").capture = Capture::Off;
        crate::console::reconsider(&page);
        steer(&cdp, "Test.release", json!({"method": "Target.attachToTarget"})).await;
        assert!(eventually(|| detached().contains(&"console-3".to_owned())).await, "{:?}", detached());
        assert_eq!(enabled(), ["console-1", "console-2"]);
        assert_eq!(console_session(&page), None);
        assert!(!page.console.lock().expect("console lock").active);

        // A failed attach leaves capture off, so the next decision attaches again.
        let refused = json!({"method": "Target.attachToTarget", "message": "No target with given id found"});
        steer(&cdp, "Test.fail", refused).await;
        capture(&page, Capture::On).await.expect_err("the attach fails");
        assert!(!page.console.lock().expect("console lock").active);
        steer(&cdp, "Test.pass", json!({"method": "Target.attachToTarget"})).await;
        crate::console::apply(&page).await.expect("capture on after the failure");
        assert_eq!(console_session(&page).as_deref(), Some("console-5"));
    }

    #[tokio::test]
    async fn only_the_console_session_is_captured() {
        let (page, cdp, _) = attached_page().await;
        capture(&page, crate::console::Capture::On).await.expect("capture on");
        let logged = |session: &str, text: &str| {
            json!({"method": "Runtime.consoleAPICalled", "sessionId": session,
                   "params": {"type": "log", "args": [{"type": "string", "value": text}]}})
        };
        let context = |session: &str, id: i64| {
            json!({"method": "Runtime.executionContextCreated", "sessionId": session,
                   "params": {"context": {"id": id, "origin": "", "name": "", "auxData": {"frameId": "main", "type": "default"}}}})
        };
        // `js --world` enables Runtime on the page session, which replays the page's messages.
        steer(&cdp, "Test.emit", logged("page", "replayed to js --world")).await;
        steer(&cdp, "Test.emit", context("console-1", 1)).await;
        steer(&cdp, "Test.emit", context("page", 2)).await;
        steer(&cdp, "Test.emit", logged("console-1", "captured")).await;
        let texts = || -> Vec<String> {
            page.console.lock().expect("console lock").entries.iter().map(|entry| entry.text.clone()).collect()
        };
        assert!(eventually(|| !texts().is_empty()).await);
        assert_eq!(texts(), ["captured"]);
        let contexts: Vec<i64> = page.execution_contexts().iter().map(|context| context.id).collect();
        assert_eq!(contexts, [2]);
    }

    #[tokio::test]
    async fn navigations_that_never_start_are_not_pending() {
        let (page, cdp, _) = attached_page().await;
        let invalid = json!({"method": "Page.navigate", "message": "Cannot navigate to invalid URL"});
        steer(&cdp, "Test.fail", invalid).await;
        let error = page.goto("http://localhost:1/", LoadState::Commit, Duration::from_secs(1)).await;
        assert!(error.is_err());
        assert_eq!(page.urls().1, None);
        steer(&cdp, "Test.pass", json!({"method": "Page.navigate"})).await;

        // Cancelled while the browser has not answered or started it.
        steer(&cdp, "Test.hold", json!({"method": "Page.navigate"})).await;
        let goto = page.goto("http://localhost:1/", LoadState::Commit, Duration::from_secs(1));
        assert!(tokio::time::timeout(Duration::from_millis(50), goto).await.is_err());
        assert!(eventually(|| page.urls().1.is_none()).await, "{:?}", page.urls());
    }

    #[tokio::test]
    async fn gone_frames_and_lost_documents_classify() {
        let (page, cdp, _) = attached_page().await;
        let gone = page.resolve("f7e2", Duration::ZERO).await.expect_err("frame 7 never existed");
        assert!(crate::inspect::not_found(&gone), "{gone:#}");
        let destroyed = json!({"method": "Runtime.evaluate", "message": "Execution context was destroyed."});
        steer(&cdp, "Test.fail", destroyed).await;
        let navigated = page.evaluate("1", false).await.expect_err("the context was destroyed");
        assert!(crate::inspect::retryable(&navigated), "{navigated:#}");
        // The world's bootstrap runs in a context the next document already destroyed.
        let replaced = json!({"method": "Runtime.evaluate", "message": "Cannot find context with specified id"});
        steer(&cdp, "Test.fail", replaced).await;
        let navigated = page.main_world().await.expect_err("the world's context is gone");
        assert!(crate::inspect::retryable(&navigated), "{navigated:#}");
    }

    #[test]
    fn refs_parse_with_and_without_frames() {
        assert_eq!(parse_ref("e12"), Some((0, "e12".into())));
        assert_eq!(parse_ref("@e3"), Some((0, "e3".into())));
        assert_eq!(parse_ref("ref=e3"), Some((0, "e3".into())));
        assert_eq!(parse_ref("f2e15"), Some((2, "f2e15".into())));
        assert_eq!(parse_ref("e"), None);
        assert_eq!(parse_ref("#e12"), None);
        assert_eq!(parse_ref("button"), None);
        assert_eq!(parse_ref("fe1"), None);
    }

    #[test]
    fn exception_messages_drop_stacks() {
        let details = ExceptionDetails {
            text: "Uncaught".into(),
            exception: Some(json!({"description": "Error: boom\n    at foo (x.js:1:2)"})),
            line_number: 0,
            url: None,
        };
        assert_eq!(details.message(), "Error: boom");
    }
}
