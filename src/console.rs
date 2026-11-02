//! Console messages and uncaught exceptions.
//!
//! Capturing them needs `Runtime.enable`, which anti-bot scripts can detect. It is therefore on
//! automatically only while the tab shows a local page and is not navigating away from it
//! (localhost, files, extensions), and explicit elsewhere. Enabling replays what the document
//! logged before, so turning it on once a local document commits loses nothing.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::cdp::{CALL_TIMEOUT, Event};
use crate::cli::ConsoleLevel;
use crate::page::Page;

/// Messages kept per tab.
const CAPACITY: usize = 1000;

/// Characters of one message shown by `hba console` and `hba errors`.
const MAX_TEXT: usize = 4000;

/// When the tab captures console messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Capture {
    /// On for local pages only.
    #[default]
    Auto,
    /// On everywhere (`hba console --on`).
    On,
    /// Off everywhere (`hba console --off`).
    Off,
}

/// One console message or uncaught exception.
#[derive(Debug, Clone)]
pub struct Entry {
    pub seq: u64,
    pub level: Level,
    pub text: String,
    pub location: String,
    /// Uncaught exception rather than a console call.
    pub exception: bool,
    pub at: Instant,
}

impl Entry {
    /// `error boom (app.js:12)`, continuation lines indented by two spaces.
    pub fn line(&self) -> String {
        let text = crate::output::truncate(&self.text, MAX_TEXT);
        let mut lines = text.lines();
        let mut out = format!("{} {}", self.level.label(), lines.next().unwrap_or_default());
        if !self.location.is_empty() {
            let _ = write!(out, " ({})", self.location);
        }
        for line in lines {
            out.push_str("\n  ");
            out.push_str(line);
        }
        out
    }

    /// The machine-readable form printed with `--json`.
    pub fn json(&self) -> Value {
        json!({
            "level": self.level.label(),
            "text": self.text,
            "location": self.location,
            "exception": self.exception,
        })
    }
}

/// Console severity, least severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Log,
    Info,
    Warn,
    Error,
}

impl Level {
    /// The name printed before each message.
    pub fn label(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Log => "log",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

impl From<ConsoleLevel> for Level {
    fn from(level: ConsoleLevel) -> Self {
        match level {
            ConsoleLevel::Debug => Level::Debug,
            ConsoleLevel::Log => Level::Log,
            ConsoleLevel::Info => Level::Info,
            ConsoleLevel::Warn => Level::Warn,
            ConsoleLevel::Error => Level::Error,
        }
    }
}

/// The tab's captured messages (latest [`CAPACITY`]) and capture state.
#[derive(Debug, Default)]
pub struct Log {
    pub capture: Capture,
    /// Whether capture is on: hba's console session is attached to the page, or attaching.
    pub active: bool,
    /// hba's own session on the page target, with `Runtime` and `Log` enabled.
    session: Option<Arc<str>>,
    /// The console session detached last: its events still queued were captured all the same.
    detached: Option<Arc<str>>,
    /// Counts capture decisions, so an attach that completes after a newer one is undone.
    turn: u64,
    /// The main frame's current document (its loader id).
    document: Option<String>,
    /// Per recent document, until when its messages were captured, in milliseconds since the
    /// Unix epoch: when capture went off for it, or when the tab left it while capturing.
    /// Attaching again replays what a document logged, and so does a document coming back from
    /// the back/forward cache while capture is on; messages up to then were captured already.
    captured_until: Vec<(String, f64)>,
    /// A history navigation stopped loading and has not committed yet: a document coming back
    /// from the back/forward cache replays its messages before the tab reports which it is.
    restoring: bool,
    pub entries: VecDeque<Entry>,
    seq: u64,
}

impl Log {
    /// Counter of messages so far; action reports list errors after a [`crate::page::Mark`].
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Forgets every message.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The main frame now shows the document of `loader`.
    pub fn set_document(&mut self, loader: Option<String>) {
        if self.session.is_some() && self.document != loader {
            self.mark_captured();
        }
        self.document = loader;
        self.restoring = false;
    }

    /// Whether a history navigation stopped loading without committing yet, possibly bringing
    /// back a document from the back/forward cache (until the main frame commits or navigates).
    pub fn set_restoring(&mut self, restoring: bool) {
        self.restoring = restoring;
    }

    /// What the current document logged until now was captured: capture goes off for it, or
    /// the tab leaves it.
    fn mark_captured(&mut self) {
        let Some(document) = self.document.clone() else { return };
        self.captured_until.retain(|(seen, _)| *seen != document);
        if self.captured_until.len() == 8 {
            self.captured_until.remove(0);
        }
        self.captured_until.push((document, epoch_millis()));
    }

    /// Whether a message the current document logged at `timestamp` (epoch milliseconds) was
    /// captured already. A document coming back from the back/forward cache replays its
    /// messages before its commit is known; whatever was logged before capture last let go of
    /// a document was captured then, while newer messages (its `pageshow`, the departing page's
    /// `pagehide`) are logged after it.
    fn captured(&self, timestamp: Option<f64>) -> bool {
        let Some(at) = timestamp else { return false };
        if self.restoring {
            return self.captured_until.iter().any(|(_, until)| at <= *until);
        }
        let document = self.document.as_deref();
        self.captured_until.iter().any(|(seen, until)| Some(seen.as_str()) == document && at <= *until)
    }

    /// Forgets errors and exceptions, keeping other messages; returns how many were removed.
    pub fn clear_errors(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.level != Level::Error);
        before - self.entries.len()
    }

    /// Records a message, dropping the oldest beyond [`CAPACITY`].
    pub fn push(&mut self, level: Level, text: String, location: String, exception: bool) {
        self.seq += 1;
        if self.entries.len() == CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry { seq: self.seq, level, text, location, exception, at: Instant::now() });
    }

    /// Up to three errors logged since `seq`, for action reports.
    pub fn report_since(&self, seq: u64, out: &mut String) {
        let errors: Vec<&Entry> = self.entries.iter().filter(|e| e.seq > seq && e.level == Level::Error).collect();
        for entry in errors.iter().take(3) {
            let text = crate::output::truncate(first_line(&entry.text), 200);
            if entry.exception {
                let _ = writeln!(out, "! {text}");
            } else {
                let _ = writeln!(out, "! console.error: {text}");
            }
        }
        if errors.len() > 3 {
            let _ = writeln!(out, "! … {} more errors (`hba errors`)", errors.len() - 3);
        }
    }

    /// Whether capture belongs on a tab showing `url` and navigating to `pending`.
    fn wanted(&self, url: &str, pending: Option<&str>) -> bool {
        match self.capture {
            Capture::On => true,
            Capture::Off => false,
            Capture::Auto => is_local_url(url) && pending.is_none_or(is_local_url),
        }
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// Now in milliseconds since the Unix epoch, the clock of the protocol's console timestamps.
fn epoch_millis() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |since| since.as_secs_f64() * 1000.0)
}

/// Pages where detection does not matter: the developer's own and the browser's. A `blob:`
/// document belongs to the origin that made it; `about:blank` can be scripted by its opener.
pub fn is_local_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    match scheme {
        "file" | "chrome-extension" | "data" | "chrome" | "devtools" => true,
        "blob" => is_local_url(rest),
        "http" | "https" | "ws" | "wss" => {
            let authority = rest.trim_start_matches('/').split(['/', '?', '#']).next().unwrap_or("");
            let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
            let host = if host.starts_with('[') {
                host.split_once(']').map_or(host, |(h, _)| &h[1..])
            } else {
                host.rsplit_once(':').map_or(host, |(h, _)| h)
            };
            host == "localhost"
                || host.ends_with(".localhost")
                || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
                || host.rsplit_once('.').is_some_and(|(_, tld)| tld == "test")
        }
        _ => false,
    }
}

/// Turns capture on or off to match the policy and where the main frame is and is going, and
/// waits for the browser to confirm.
pub async fn apply(page: &Page) -> Result<()> {
    let Some(task) = switch(page)? else { return Ok(()) };
    // A dialog cuts the wait short; the task still finishes on its own.
    page.guarded(async { task.await.context("switching console capture")? }).await
}

/// [`apply`] without waiting, for page events: a navigation started, redirected, committed or
/// ended without committing.
pub fn reconsider(page: &Page) {
    let _ = switch(page);
}

/// Capture lives on a session of its own, attached to the page target, rather than on the page
/// session: while a navigation is pending the browser holds back commands for the renderer
/// (such as `Runtime.disable`) until the next document has started with the session's old
/// state, but detaching a session takes effect at once.
///
/// Decides at once and starts the work in a task, so the latest decision wins.
fn switch(page: &Page) -> Result<Option<JoinHandle<Result<()>>>> {
    let (url, pending) = page.urls();
    let target = page.page_target();
    let mut log = page.console.lock().expect("console lock");
    let want = log.wanted(&url, pending.as_deref());
    if want == log.active {
        return Ok(None);
    }
    crate::trace!("console capture {} for {}", if want { "on" } else { "off" }, pending.as_deref().unwrap_or(&url));
    if want {
        let target = target.context("the tab has no page right now; retry")?;
        let attach = page.cdp.start(
            "Target.attachToTarget",
            &json!({"targetId": target, "flatten": true}),
            None,
            CALL_TIMEOUT,
        )?;
        log.active = true;
        log.turn += 1;
        let turn = log.turn;
        let page = page.arc();
        return Ok(Some(tokio::spawn(async move { attached(&page, turn, attach.await).await })));
    }
    log.active = false;
    log.turn += 1;
    log.mark_captured();
    let session = log.session.take();
    log.detached.clone_from(&session);
    drop(log);
    let Some(session) = session else { return Ok(None) };
    page.cdp.unroute(&session);
    page.forget_contexts(&session);
    let detach = page.cdp.start("Target.detachFromTarget", &json!({"sessionId": &*session}), None, CALL_TIMEOUT)?;
    Ok(Some(tokio::spawn(async move { detach.await.map(drop) })))
}

/// Finishes turning capture on once hba's console session is attached, unless capture went
/// off meanwhile.
async fn attached(page: &Page, turn: u64, reply: Result<Box<RawValue>>) -> Result<()> {
    let current = || page.console.lock().expect("console lock").turn == turn;
    let attached = reply.and_then(|raw| Ok(serde_json::from_str::<crate::page::Attached>(raw.get())?));
    let session: Arc<str> = match attached {
        Ok(attached) => Arc::from(attached.session_id),
        Err(error) => {
            if current() {
                page.console.lock().expect("console lock").active = false;
            }
            return Err(error.context("attaching hba's console session"));
        }
    };
    if !current() || !page.route(&session) {
        page.cdp.send("Target.detachFromTarget", json!({"sessionId": &*session}), None).await.ok();
        return Ok(());
    }
    page.console.lock().expect("console lock").session = Some(Arc::clone(&session));
    let (runtime, events) = tokio::join!(
        page.cdp.send("Runtime.enable", json!({}), Some(&session)),
        page.cdp.send("Log.enable", json!({}), Some(&session)),
    );
    runtime.and(events)
}

/// hba's console session, if attached.
pub fn session(page: &Page) -> Option<Arc<str>> {
    page.console.lock().expect("console lock").session.clone()
}

/// Turns capture off and detaches hba's console session, when `hba` lets go of the tab or the
/// tab gets a new page.
pub async fn release(page: &Page) {
    let session = {
        let mut log = page.console.lock().expect("console lock");
        log.active = false;
        log.turn += 1;
        let session = log.session.take();
        log.detached.clone_from(&session);
        session
    };
    if let Some(session) = session {
        page.cdp.unroute(&session);
        page.forget_contexts(&session);
        page.cdp.send("Target.detachFromTarget", json!({"sessionId": &*session}), None).await.ok();
    }
}

/// Called after a page or frame session is prepared, and when a prerendered page takes over:
/// a new page needs a console session of its own.
pub async fn on_session(page: &Page, session: &str) {
    if !page.session().is_ok_and(|primary| &*primary == session) {
        return;
    }
    release(page).await;
    apply(page).await.ok();
}

/// Records `Runtime.*` and `Log.*` events of hba's console session. `js --world` turns `Runtime`
/// on for the page session too; what that reports is not capture.
pub fn on_event(page: &Page, event: &Event) {
    let mut log = page.console.lock().expect("console lock");
    let Some(session) = event.session.as_deref() else { return };
    if log.session.as_deref() == Some(session) || log.detached.as_deref() == Some(session) {
        record(&mut log, event);
    }
}

/// Adds a console call (`Runtime.consoleAPICalled`), uncaught exception
/// (`Runtime.exceptionThrown`) or browser message (`Log.entryAdded`) to `log`; ignores other events
/// and replays of messages captured already.
pub fn record(log: &mut Log, event: &Event) {
    match event.method.as_str() {
        "Runtime.consoleAPICalled" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Called {
                #[serde(rename = "type")]
                kind: String,
                #[serde(default)]
                args: Vec<Value>,
                #[serde(default)]
                stack_trace: Option<Value>,
                #[serde(default)]
                timestamp: Option<f64>,
            }
            let Ok(called) = event.parse::<Called>() else {
                return;
            };
            if log.captured(called.timestamp) {
                return;
            }
            let level = match called.kind.as_str() {
                "error" | "assert" => Level::Error,
                "warning" => Level::Warn,
                "info" => Level::Info,
                "debug" => Level::Debug,
                _ => Level::Log,
            };
            let text = called.args.iter().map(format_remote).collect::<Vec<_>>().join(" ");
            let location = called.stack_trace.as_ref().map(top_frame).unwrap_or_default();
            log.push(level, text, location, false);
        }
        "Runtime.exceptionThrown" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Thrown {
                #[serde(default)]
                timestamp: Option<f64>,
                exception_details: crate::page::ExceptionDetails,
            }
            let Ok(thrown) = event.parse::<Thrown>() else {
                return;
            };
            if log.captured(thrown.timestamp) {
                return;
            }
            let details = thrown.exception_details;
            let location = details
                .url
                .as_deref()
                .map(|url| format!("{}:{}", short_source(url), details.line_number + 1))
                .unwrap_or_default();
            // `text` says how it escaped ("Uncaught", "Uncaught (in promise)"); the message says what.
            let message = details.message();
            let text = if details.text.is_empty() || message.starts_with(&details.text) {
                message
            } else {
                format!("{} {message}", details.text)
            };
            log.push(Level::Error, text, location, true);
        }
        "Log.entryAdded" => {
            #[derive(Deserialize)]
            struct Added {
                entry: LogEntry,
            }
            #[derive(Deserialize)]
            struct LogEntry {
                source: String,
                level: String,
                text: String,
                #[serde(default)]
                url: Option<String>,
                #[serde(default)]
                timestamp: Option<f64>,
            }
            let Ok(Added { entry }) = event.parse::<Added>() else {
                return;
            };
            // Failed loads are in the network log already (`hba net --failed`), favicons included.
            if entry.source == "network" || log.captured(entry.timestamp) {
                return;
            }
            let level = match entry.level.as_str() {
                "error" => Level::Error,
                "warning" => Level::Warn,
                "verbose" => Level::Debug,
                _ => Level::Info,
            };
            let location = entry.url.as_deref().map(short_source).unwrap_or_default();
            log.push(level, entry.text, location, false);
        }
        _ => {}
    }
}

/// A console argument as the browser's console would print it, compactly.
pub fn format_remote(arg: &Value) -> String {
    if let Some(value) = arg.get("value") {
        return match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
    }
    if let Some(value) = arg.get("unserializableValue").and_then(Value::as_str) {
        return value.to_owned();
    }
    if let Some(preview) = arg.get("preview") {
        return format_preview(preview);
    }
    arg.get("description").and_then(Value::as_str).map_or_else(|| "undefined".to_owned(), str::to_owned)
}

fn format_preview(preview: &Value) -> String {
    let properties = preview.get("properties").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let array = preview.get("subtype").and_then(Value::as_str) == Some("array");
    let items: Vec<String> = properties
        .iter()
        .map(|p| {
            let value = p.get("value").and_then(Value::as_str).unwrap_or("…");
            if array {
                value.to_owned()
            } else {
                format!("{}: {value}", p.get("name").and_then(Value::as_str).unwrap_or("?"))
            }
        })
        .collect();
    let more = if preview.get("overflow").and_then(Value::as_bool).unwrap_or(false) { ", …" } else { "" };
    if array { format!("[{}{more}]", items.join(", ")) } else { format!("{{{}{more}}}", items.join(", ")) }
}

fn top_frame(stack: &Value) -> String {
    let Some(frame) = stack.get("callFrames").and_then(Value::as_array).and_then(|frames| frames.first()) else {
        return String::new();
    };
    let url = frame.get("url").and_then(Value::as_str).unwrap_or_default();
    let line = frame.get("lineNumber").and_then(Value::as_i64).unwrap_or(0) + 1;
    if url.is_empty() { String::new() } else { format!("{}:{line}", short_source(url)) }
}

/// Last path segment of a script URL, without the query.
pub fn short_source(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').find(|part| !part.is_empty()).unwrap_or(path).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_urls_are_recognized() {
        for url in [
            "http://localhost:3000/",
            "https://app.localhost/x",
            "http://127.0.0.1:8080",
            "http://127.8.9.10/",
            "http://[::1]:5173/",
            "http://0.0.0.0:3000/",
            "http://app.test/",
            "file:///tmp/a.html",
            "chrome-extension://abc/popup.html",
            "http://user@localhost/",
            "data:text/html,<p>hi",
            "blob:http://localhost:3000/6c1f9a2e-0d1b-4c55-9f4e-8a1d2b3c4d5e",
            "blob:file:///tmp/6c1f9a2e",
        ] {
            assert!(is_local_url(url), "{url}");
        }
        for url in [
            "https://example.com",
            "https://localhost.evil.com/",
            "http://192.168.1.2/",
            "javascript:alert(1)",
            "about:blank",
            "about:srcdoc",
            "blob:https://example.com/6c1f9a2e-0d1b-4c55-9f4e-8a1d2b3c4d5e",
            "blob:null/6c1f9a2e",
            "https://127.0.0.1.attacker.example/",
            "http://127.evil.com/",
            "http://[::2]/",
        ] {
            assert!(!is_local_url(url), "{url}");
        }
    }

    #[test]
    fn capture_needs_a_local_page_that_stays_local() {
        let log = Log::default();
        assert!(log.wanted("http://localhost:3000/", None));
        assert!(log.wanted("http://localhost:3000/", Some("http://localhost:3000/next")));
        // Leaving for a website turns capture off before the website's document exists.
        assert!(!log.wanted("http://localhost:3000/", Some("https://accounts.example.com/login")));
        // Arriving from a website turns it on only once the local document commits.
        assert!(!log.wanted("https://example.com/", Some("http://localhost:3000/")));
        assert!(!log.wanted("about:blank", None));
        let on = Log { capture: Capture::On, ..Log::default() };
        assert!(on.wanted("https://example.com/", None));
        let off = Log { capture: Capture::Off, ..Log::default() };
        assert!(!off.wanted("http://localhost:3000/", None));
    }

    #[test]
    fn remote_values_format_like_devtools() {
        assert_eq!(format_remote(&json!({"type": "string", "value": "hi"})), "hi");
        assert_eq!(format_remote(&json!({"type": "number", "value": 3})), "3");
        assert_eq!(format_remote(&json!({"type": "number", "unserializableValue": "NaN"})), "NaN");
        let object =
            json!({"type": "object", "preview": {"properties": [{"name": "a", "value": "1"}], "overflow": true}});
        assert_eq!(format_remote(&object), "{a: 1, …}");
    }

    #[test]
    fn reports_cap_at_three_errors() {
        let mut log = Log::default();
        for n in 0..5 {
            log.push(Level::Error, format!("Uncaught Error: boom {n}\n    at x"), String::new(), true);
        }
        log.push(Level::Log, "fine".into(), String::new(), false);
        let mut out = String::new();
        log.report_since(1, &mut out);
        assert_eq!(out.lines().count(), 4, "{out}");
        assert!(out.starts_with("! Uncaught Error: boom 1\n"), "{out}");
    }

    #[test]
    fn entries_print_level_text_and_location() {
        let mut log = Log::default();
        log.push(Level::Warn, "first\nsecond".into(), "app.js:3".into(), false);
        log.push(Level::Log, "plain".into(), String::new(), false);
        let lines: Vec<String> = log.entries.iter().map(Entry::line).collect();
        assert_eq!(lines, ["warn first (app.js:3)\n  second", "log plain"]);
    }

    #[test]
    fn clearing_errors_keeps_other_messages() {
        let mut log = Log::default();
        log.push(Level::Error, "bad".into(), String::new(), true);
        log.push(Level::Info, "fine".into(), String::new(), false);
        assert_eq!(log.clear_errors(), 1);
        assert_eq!(log.entries.len(), 1);
        assert_eq!(log.entries[0].text, "fine");
    }

    #[test]
    fn replays_of_captured_messages_are_skipped() {
        let logged = |text: &str, at: f64| Event {
            session: None,
            method: "Runtime.consoleAPICalled".to_owned(),
            params: RawValue::from_string(
                json!({"type": "log", "args": [{"type": "string", "value": text}], "timestamp": at}).to_string(),
            )
            .expect("valid JSON"),
        };
        let mut log = Log::default();
        log.set_document(Some("A".to_owned()));
        log.mark_captured();
        let (before, after) = (epoch_millis() - 60_000.0, epoch_millis() + 60_000.0);
        record(&mut log, &logged("captured before capture went off", before));
        record(&mut log, &logged("logged while off", after));
        log.set_document(Some("B".to_owned()));
        record(&mut log, &logged("another document", before));
        let texts: Vec<&str> = log.entries.iter().map(|entry| entry.text.as_str()).collect();
        assert_eq!(texts, ["logged while off", "another document"]);

        // A document the tab left while capturing comes back from the back/forward cache. It
        // replays what it logged before the tab reports the commit; the departing page and the
        // restored one keep logging meanwhile.
        let mut log = Log { session: Some(Arc::from("console")), ..Log::default() };
        log.set_document(Some("A".to_owned()));
        record(&mut log, &logged("A while shown", before));
        log.set_document(Some("B".to_owned()));
        log.set_restoring(true);
        record(&mut log, &logged("A while shown", before));
        record(&mut log, &logged("B pagehide", after));
        record(&mut log, &logged("A pageshow", after));
        log.set_document(Some("A".to_owned()));
        record(&mut log, &logged("A while shown", before));
        let texts: Vec<&str> = log.entries.iter().map(|entry| entry.text.as_str()).collect();
        assert_eq!(texts, ["A while shown", "B pagehide", "A pageshow"]);
    }
}
