//! One attached tab: its page and frame sessions, isolated worlds, navigation and dialogs.
//!
//! Attachment goes through the tab target, so the page survives prerender activations and
//! process swaps. `Runtime` stays disabled on websites (it is an automation tell); `hba`'s own
//! code runs in an isolated world and page JavaScript runs without enabling any domain.

mod events;
mod resolve;
mod wait;
mod world;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, watch};

use crate::cdp::Cdp;
use crate::cli::LoadState;
use crate::config::Humanize;
use crate::injected::Source;
use crate::snapshot::Baseline;
use crate::undo::Undo;

pub use resolve::parse_ref;
pub use wait::Deadline;
pub use world::{ExceptionDetails, evaluate_on};

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

/// The element a target names is not on the page; [`crate::inspect::not_found`] recognizes it.
#[derive(Debug)]
pub struct NotFound(String);

impl fmt::Display for NotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotFound {}

/// The page was between documents, or without one for a moment, so asking again will answer;
/// [`crate::inspect::retryable`] recognizes it.
#[derive(Debug)]
pub struct Retry(pub &'static str);

impl fmt::Display for Retry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}; retry", self.0)
    }
}

impl std::error::Error for Retry {}

/// Whether attaching to the tab worked, once its first page is prepared: `None` until then,
/// else the error that preparing it ended with.
type Readiness = Option<Result<(), String>>;

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
    /// The last snapshot, which `snap -d` compares against.
    last_snapshot: Option<Baseline>,
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

    /// Forgets frames that are gone, with what refers to them.
    fn drop_frames(&mut self, gone: impl Fn(&str, &Frame) -> bool) {
        let State { frames, snapshotted, .. } = self;
        frames.retain(|id, frame| {
            let keep = !gone(id, frame);
            if !keep {
                snapshotted.remove(id);
            }
            keep
        });
    }
}

pub struct Page {
    /// This page, for tasks that outlive a call.
    me: Weak<Page>,
    pub tab: String,
    pub cdp: Cdp,
    source: Arc<Source>,
    /// Follows the configuration, which may change while the page is attached.
    humanize: Mutex<Humanize>,
    tab_session: String,
    /// The event loop's channel. Only routed sessions hold it strongly: once every session is
    /// unrouted the channel closes and the loop ends, freeing the page.
    events: mpsc::WeakUnboundedSender<crate::cdp::Event>,
    state: Mutex<State>,
    signals: broadcast::Sender<Signal>,
    ready: watch::Sender<Readiness>,
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
        let (ready, mut ready_rx) = watch::channel(None);
        let page = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            tab: tab.to_owned(),
            cdp: cdp.clone(),
            source,
            humanize: Mutex::new(humanize),
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
        let ready = async {
            cdp.send(
                "Target.setAutoAttach",
                json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
                Some(&attached.session_id),
            )
            .await?;
            let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx.wait_for(Option::is_some)).await;
            match ready.ok().and_then(Result::ok).map(|ready| Readiness::clone(&ready)) {
                Some(Some(Ok(()))) => Ok(()),
                Some(Some(Err(error))) => bail!("tab {tab} could not be prepared: {error}"),
                _ => bail!("tab {tab} did not expose a page to attach to"),
            }
        }
        .await;
        if let Err(error) = ready {
            undo.finish().await;
            return Err(error);
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
                        && let Ok(attached) = event.parse::<events::AttachedEvent>()
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

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("page lock")
    }

    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire) && !self.cdp.is_closed()
    }

    /// Marks the page gone and stops routing its sessions' events, which ends its event loop.
    pub fn mark_detached(&self) {
        self.live.store(false, Ordering::Release);
        let sessions: Vec<Arc<str>> = {
            let state = self.state();
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
        Ok(self.state().primary.clone().ok_or(Retry("the tab has no page right now"))?)
    }

    pub fn page_target(&self) -> Option<String> {
        self.state().page_target.clone()
    }

    pub fn url(&self) -> String {
        self.state().url.clone()
    }

    /// The current document's URL and the URL the main frame is navigating to, if any.
    pub fn urls(&self) -> (String, Option<String>) {
        let state = self.state();
        (state.url.clone(), state.pending.clone())
    }

    /// The main frame's document request was redirected to `url` before committing.
    pub fn on_document_redirect(&self, frame: &str, url: &str) {
        let mut state = self.state();
        if state.main_frame.as_deref() != Some(frame) || state.pending.is_none() {
            return;
        }
        state.pending = Some(url.to_owned());
        drop(state);
        crate::console::reconsider(self);
    }

    pub fn navigations(&self) -> u64 {
        self.state().navigations
    }

    pub fn main_frame(&self) -> Result<String> {
        self.state().main_frame.clone().context("the page has no main frame yet")
    }

    pub fn dialog(&self) -> Option<Dialog> {
        self.state().dialog.clone()
    }

    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.state().notes)
    }

    pub fn mouse(&self) -> (f64, f64) {
        self.state().mouse
    }

    pub fn set_mouse(&self, point: (f64, f64)) {
        self.state().mouse = point;
    }

    /// Mouse buttons held down, as the CDP `buttons` bitmask.
    pub fn buttons(&self) -> u8 {
        self.state().buttons
    }

    pub fn set_buttons(&self, buttons: u8) {
        self.state().buttons = buttons;
    }

    /// Contexts seen while `Runtime` is enabled on the tab (none otherwise).
    pub fn execution_contexts(&self) -> Vec<ExecutionContext> {
        self.state().contexts.clone()
    }

    /// Forgets the execution contexts recorded for `session`, after `Runtime.disable`: disabling
    /// reports nothing, and enabling again reports every context anew.
    pub fn forget_contexts(&self, session: &str) {
        self.state().contexts.retain(|context| &*context.session != session);
    }

    pub fn last_snapshot(&self) -> Option<Baseline> {
        self.state().last_snapshot.clone()
    }

    /// Keeps `lines`, a snapshot of the current document taken with `options` (of the subtree
    /// of ref `scope`), for `snap -d` to compare against.
    pub fn remember_snapshot(&self, options: crate::snapshot::Options, scope: Option<String>, lines: Vec<String>) {
        let mut state = self.state();
        state.last_snapshot = Some(Baseline { loader: state.loader.clone(), options, scope, lines });
    }

    /// Records that refs in `world`'s frame now come from this document.
    pub fn note_snapshot(&self, world: &World) {
        self.state().snapshotted.insert(world.frame.clone(), world.context);
    }

    /// `hba`'s world in `frame` if the frame's last snapshot came from it. Refs of any other
    /// world are stale: a frame's world goes away when its document does.
    fn snapshot_world(&self, frame: &str) -> Option<World> {
        let state = self.state();
        let world = state.frames.get(frame)?.world.clone()?;
        (state.snapshotted.get(frame) == Some(&world.context)).then_some(world)
    }

    pub fn loader(&self) -> Option<String> {
        self.state().loader.clone()
    }

    /// Frame id for a ref prefix sequence (0 = main frame).
    pub fn frame_by_seq(&self, seq: u32) -> Option<String> {
        let state = self.state();
        if seq == 0 {
            return state.main_frame.clone();
        }
        state.frames.iter().find(|(_, frame)| frame.seq == seq).map(|(id, _)| id.clone())
    }

    pub fn frame_parent(&self, frame: &str) -> Option<String> {
        self.state().frames.get(frame).and_then(|f| f.parent.clone())
    }

    pub fn frame_session(&self, frame: &str) -> Option<Arc<str>> {
        self.state().frames.get(frame).map(|f| Arc::clone(&f.session))
    }

    /// Live page and out-of-process frame sessions of the tab, each once; `true` marks page sessions.
    pub fn sessions(&self) -> Vec<(Arc<str>, bool)> {
        let state = self.state();
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
        match *self.humanize.lock().expect("humanize lock") {
            Humanize::Always => true,
            Humanize::Never => false,
            Humanize::Auto => !self.is_local(),
        }
    }

    /// Follows a changed `humanize` setting.
    pub fn set_humanize(&self, humanize: Humanize) {
        *self.humanize.lock().expect("humanize lock") = humanize;
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
        self.state().dialog = None;
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

    /// Reads this tab's profile directory from `chrome://version` (used once per profile).
    pub async fn profile_directory(&self) -> Result<String> {
        self.goto("chrome://version/", LoadState::Load, Duration::from_secs(10)).await?;
        let path = self.evaluate("document.getElementById('profile_path')?.textContent ?? ''", false).await?;
        let path = path.as_str().unwrap_or_default().trim().to_owned();
        let dir = std::path::Path::new(&path).file_name().and_then(|name| name.to_str()).map(str::to_owned);
        dir.with_context(|| format!("chrome://version shows no profile path ({path:?})"))
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
    async fn a_page_that_cannot_be_prepared_is_not_attached() {
        let (url, received) = fake_browser().await;
        let (cdp, _browser_events) = Cdp::connect(&url).await.expect("connect");
        steer(&cdp, "Test.fail", json!({"method": "Page.enable", "message": "Page.enable failed"})).await;
        let attach = Page::attach(cdp.clone(), Arc::new(Source::new()), Humanize::Never, "T").await;
        let error = attach.err().expect("a page without its frame tree is no page to drive");
        assert!(format!("{error:#}").contains("Page.enable failed"), "{error:#}");
        assert!(sessions_of(&received, "Target.detachFromTarget").contains(&"tab".to_owned()));
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
    async fn one_deadline_bounds_every_phase_of_a_navigation() {
        let (page, cdp, _) = attached_page().await;
        // The browser takes the whole timeout to answer; no phase after it gets a fresh one.
        steer(&cdp, "Test.hold", json!({"method": "Page.navigate"})).await;
        let started = Instant::now();
        let error = page.goto("http://localhost:1/", LoadState::Idle, Duration::from_millis(200)).await;
        let error = error.expect_err("the browser never answered");
        assert!(started.elapsed() < Duration::from_millis(600), "took {:?}", started.elapsed());
        let message = format!("{error:#}");
        assert!(message.contains("timed out after 0.2s waiting for http://localhost:1/ to answer"), "{message}");
    }

    #[tokio::test]
    async fn a_selector_wait_outlasts_a_navigation() {
        let (page, cdp, _) = attached_page().await;
        // Every document the wait asks is replaced before it answers, as during a redirect.
        let replaced = json!({"method": "Runtime.evaluate", "message": "Cannot find context with specified id"});
        steer(&cdp, "Test.fail", replaced).await;
        let started = Instant::now();
        let error = page.resolve("text=Dashboard", Duration::from_millis(300)).await.expect_err("never appears");
        assert!(crate::inspect::not_found(&error), "{error:#}");
        assert!(started.elapsed() >= Duration::from_millis(300), "gave up after {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn gone_frames_and_lost_documents_classify() {
        let (page, cdp, _) = attached_page().await;
        let gone = page.resolve("f7e2", Duration::ZERO).await.expect_err("frame 7 never existed");
        assert!(crate::inspect::not_found(&gone), "{gone:#}");
        // Typed, so the classification survives context added on the way up.
        assert!(crate::inspect::not_found(&gone.context("clicking f7e2")));
        let destroyed = json!({"method": "Runtime.evaluate", "message": "Execution context was destroyed."});
        steer(&cdp, "Test.fail", destroyed).await;
        let navigated = page.evaluate("1", false).await.expect_err("the context was destroyed");
        assert!(crate::inspect::retryable(&navigated), "{navigated:#}");
        assert!(crate::inspect::retryable(&navigated.context("reading the title")));
        // The world's bootstrap runs in a context the next document already destroyed.
        let replaced = json!({"method": "Runtime.evaluate", "message": "Cannot find context with specified id"});
        steer(&cdp, "Test.fail", replaced).await;
        let navigated = page.main_world().await.expect_err("the world's context is gone");
        assert!(crate::inspect::retryable(&navigated), "{navigated:#}");
    }

    #[tokio::test]
    async fn refs_of_detached_frames_are_forgotten() {
        let (page, cdp, _) = attached_page().await;
        let frame_event =
            |method: &str, params: Value| json!({"method": method, "sessionId": "page", "params": params});
        let attached = frame_event("Page.frameAttached", json!({"frameId": "ad", "parentFrameId": "main"}));
        steer(&cdp, "Test.emit", attached).await;
        assert!(eventually(|| page.frame_parent("ad").is_some()).await);
        let world = World { session: Arc::from("page"), frame: "ad".into(), context: 3, engine: "engine".into() };
        page.note_snapshot(&world);
        steer(&cdp, "Test.emit", frame_event("Page.frameDetached", json!({"frameId": "ad", "reason": "remove"}))).await;
        assert!(eventually(|| page.frame_parent("ad").is_none()).await);
        assert!(!page.state().snapshotted.contains_key("ad"), "rotating ad frames would grow it for good");
    }

    #[tokio::test]
    async fn refs_of_a_frame_without_its_snapshot_world_are_stale() {
        let (page, _cdp, received) = attached_page().await;
        let world = World { session: Arc::from("page"), frame: "main".into(), context: 7, engine: "engine".into() };
        page.note_snapshot(&world);
        // The frame navigated since: its world is gone, so the ref is stale without a new world.
        let error = page.resolve("e3", Duration::ZERO).await.expect_err("the ref is from an earlier page");
        assert!(format!("{error:#}").contains("from an earlier page"), "{error:#}");
        assert!(sessions_of(&received, "Page.createIsolatedWorld").is_empty(), "the engine was injected for nothing");
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
