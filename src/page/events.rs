//! The page's event loop: attached pages and frames, the frame tree, navigation, lifecycle
//! events, dialogs and execution contexts.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Dialog, ExecutionContext, Frame, Page, Signal};
use crate::cdp::Event;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AttachedEvent {
    session_id: String,
    pub(super) target_info: crate::browser::TargetInfo,
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

impl Page {
    pub(super) async fn run(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<Event>) {
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
                    let mut state = self.state();
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
                    self.state().drop_frames(|id, _| id == detached.frame_id);
                }
            }
            "Page.navigatedWithinDocument" => {
                if let Ok(within) = event.parse::<NavigatedWithinDocument>() {
                    let mut state = self.state();
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
                    let mut state = self.state();
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
                    let mut state = self.state();
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
                    let mut state = self.state();
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
                self.state().dialog = None;
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
                    let mut state = self.state();
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
                    let mut state = self.state();
                    state.contexts.retain(|c| !(c.id == destroyed.execution_context_id && c.session == *session));
                }
            }
            "Runtime.executionContextsCleared" => {
                if let Some(session) = session {
                    self.state().contexts.retain(|c| c.session != *session);
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
                    let mut state = self.state();
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
                    let mut state = self.state();
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
        let mut state = self.state();
        state.pages.retain(|_, (s, _)| &**s != session);
        if state.primary.as_deref() == Some(session) {
            state.primary = None;
            state.page_target = None;
        }
        state.drop_frames(|_, frame| &*frame.session == session);
        state.contexts.retain(|context| &*context.session != session);
    }

    /// Chromium activates a prerendered page by swapping it into the tab. Once its target loses
    /// the `prerender` subtype it is the tab's document.
    pub fn on_target_changed(self: &Arc<Self>, info: &crate::browser::TargetInfo) {
        if info.subtype.as_deref().is_some_and(|subtype| !subtype.is_empty()) {
            return;
        }
        let session = {
            let mut state = self.state();
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
                        let mut state = page.state();
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
        if primary {
            // The tab's first page decides whether attaching worked: a page without its frame
            // tree and lifecycle events is no page to drive. Later ones find it decided.
            let readiness = outcome.as_ref().map(drop).map_err(|error| format!("{error:#}"));
            self.ready.send_if_modified(|ready| {
                let first = ready.is_none();
                if first {
                    *ready = Some(readiness);
                }
                first
            });
        }
        if let Err(error) = outcome {
            eprintln!("hba: preparing page session failed: {error:#}");
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
        let mut state = self.state();
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
        let mut state = self.state();
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
        // The session that commits the document hosts the frame now: an out-of-process frame
        // that moved back into its parent's process must not go when its old session does.
        entry.session = Arc::clone(session);
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
            self.state().notes.push(format!("{kind} {message:?} (accepted)"));
            let (cdp, session) = (self.cdp.clone(), Arc::clone(session));
            tokio::spawn(async move {
                cdp.send("Page.handleJavaScriptDialog", json!({"accept": true}), Some(&session)).await.ok();
            });
        } else {
            self.state().dialog = Some(Dialog { kind, message, default_prompt });
        }
        self.signal(Signal::Dialog);
    }
}
