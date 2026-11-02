//! Network log of the tab: requests, responses, failures and in-flight tracking.
//!
//! A bounded ring buffer per tab, filled from the `Network.*` events `hba` enables on every page
//! and frame session. Short ids are consecutive, so lookups by id take constant time.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::cdp::Event;
use crate::output;
use crate::page::Page;

/// Requests kept per tab.
const CAPACITY: usize = 1000;

/// Requests in flight longer than this are treated as long-polling and ignored for settling.
const LONG_POLL: Duration = Duration::from_secs(8);

/// One request, or one hop of a redirect chain.
#[derive(Debug, Clone)]
pub struct Request {
    /// Short id shown to the user (`hba net show 12`).
    pub id: u64,
    /// The protocol's request id, shared by the hops of a redirect chain.
    pub network_id: String,
    /// Session that saw the request; its response body is fetched there.
    pub session: Arc<str>,
    pub method: String,
    pub url: String,
    /// Protocol resource type: `Document`, `Fetch`, `Script`, …
    pub kind: String,
    /// What started the request, e.g. `script app.js:34`.
    pub initiator: String,
    pub headers: Value,
    pub post_data: Option<String>,
    pub status: Option<u16>,
    pub status_text: String,
    /// `h2`, `http/1.1`, …
    pub protocol: Option<String>,
    /// Server address as `ip:port`.
    pub remote: Option<String>,
    pub mime: Option<String>,
    pub response_headers: Value,
    pub timing: Option<Timing>,
    pub failure: Option<String>,
    pub from_cache: bool,
    /// Bytes received on the wire, headers included.
    pub size: Option<u64>,
    /// Decoded body bytes received.
    pub body_size: u64,
    /// Seconds since the Unix epoch when the request was issued.
    pub wall_time: f64,
    /// Protocol monotonic timestamps in seconds: issued, and finished or failed.
    pub issued: f64,
    pub ended: Option<f64>,
    /// When `hba` saw the request, for settling after actions.
    pub started: Instant,
    pub done: bool,
}

/// Phases of a response in milliseconds after `request_time`; -1 when a phase did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timing {
    /// Protocol monotonic timestamp in seconds that the phases are relative to.
    pub request_time: f64,
    #[serde(default = "unset")]
    pub dns_start: f64,
    #[serde(default = "unset")]
    pub dns_end: f64,
    #[serde(default = "unset")]
    pub connect_start: f64,
    #[serde(default = "unset")]
    pub connect_end: f64,
    #[serde(default = "unset")]
    pub ssl_start: f64,
    #[serde(default = "unset")]
    pub ssl_end: f64,
    #[serde(default = "unset")]
    pub send_start: f64,
    #[serde(default = "unset")]
    pub send_end: f64,
    #[serde(default = "unset")]
    pub receive_headers_end: f64,
}

const fn unset() -> f64 {
    -1.0
}

/// A request that failed or answered with an HTTP error.
#[derive(Debug, Clone, Copy)]
struct Problem {
    seq: u64,
    id: u64,
    at: Instant,
}

/// Requests captured between `hba net har start` and `stop`.
#[derive(Debug)]
pub struct Recording {
    pub requests: Vec<Request>,
    /// Recorded requests no longer in the log (ring buffer overflow or `hba net clear`).
    pub dropped: u64,
}

/// The tab's requests (latest [`CAPACITY`]) and its failures.
#[derive(Debug, Default)]
pub struct Log {
    requests: VecDeque<Request>,
    by_request: HashMap<String, u64>,
    next_id: u64,
    /// Sequence of finished-with-problem events, for action reports.
    seq: u64,
    problems: VecDeque<Problem>,
    /// First request id of the HAR recording in progress.
    har: Option<u64>,
}

impl Log {
    /// Counter of failures so far; action reports list those after a [`crate::page::Mark`].
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Forgets every request and failure; ids keep counting up.
    pub fn clear(&mut self) {
        self.requests.clear();
        self.by_request.clear();
        self.problems.clear();
    }

    /// Requests, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Request> {
        self.requests.iter()
    }

    /// The request with short id `id`, if still in the log.
    pub fn get(&self, id: u64) -> Option<&Request> {
        self.requests.get(self.index(id)?)
    }

    fn index(&self, id: u64) -> Option<usize> {
        let first = self.requests.front()?.id;
        usize::try_from(id.checked_sub(first)?).ok()
    }

    fn entry(&mut self, network_id: &str) -> Option<&mut Request> {
        let index = self.index(*self.by_request.get(network_id)?)?;
        self.requests.get_mut(index)
    }

    fn push(&mut self, request: Request) {
        if self.requests.len() == CAPACITY
            && let Some(old) = self.requests.pop_front()
            && self.by_request.get(&old.network_id) == Some(&old.id)
        {
            self.by_request.remove(&old.network_id);
        }
        self.by_request.insert(request.network_id.clone(), request.id);
        self.requests.push_back(request);
    }

    /// True while document, fetch or XHR requests that started at or after `since` are
    /// unfinished. Long-lived requests are ignored after a few seconds.
    pub fn busy_since(&self, since: Instant) -> bool {
        self.requests
            .iter()
            .rev()
            .take_while(|request| request.started >= since)
            .any(|request| !request.done && breaks_page(&request.kind) && request.started.elapsed() < LONG_POLL)
    }

    /// True when a document request that started at or after `since` was abandoned after a
    /// response with content: the browser turned the navigation into a download.
    pub fn turned_into_download(&self, since: Instant) -> bool {
        self.requests.iter().rev().take_while(|request| request.started >= since).any(|request| {
            request.kind == "Document"
                && request.failure.is_some()
                && request.status.is_some_and(|status| (200..300).contains(&status) && status != 204 && status != 205)
        })
    }

    /// Up to three failed requests since `seq`, for action reports.
    pub fn report_since(&self, seq: u64, out: &mut String) {
        let failed: Vec<&Request> = self
            .problems
            .iter()
            .filter(|problem| problem.seq > seq)
            .filter_map(|problem| self.get(problem.id))
            .collect();
        for request in failed.iter().take(3) {
            let _ = writeln!(out, "! {}", request.summary());
        }
        if failed.len() > 3 {
            let _ = writeln!(out, "! … {} more failed requests (`hba net --failed`)", failed.len() - 3);
        }
    }

    /// Network failures, and HTTP errors of documents, fetches and XHRs, oldest first, with when
    /// they happened.
    pub fn failures(&self) -> impl Iterator<Item = (Instant, &Request)> {
        self.problems.iter().filter_map(|problem| Some((problem.at, self.get(problem.id)?)))
    }

    /// Forgets the failures (the requests stay in the log) and returns how many were listed.
    pub fn clear_failures(&mut self) -> usize {
        let listed = self.failures().count();
        self.problems.clear();
        listed
    }

    /// Starts recording requests for a HAR file; false when a recording is already running.
    pub fn start_har(&mut self) -> bool {
        if self.har.is_some() {
            return false;
        }
        self.har = Some(self.next_id + 1);
        true
    }

    /// The HAR recording so far, without ending it; `None` when none is running.
    pub fn har_recording(&self) -> Option<Recording> {
        let first = self.har?;
        let requests: Vec<Request> = self.requests.iter().filter(|request| request.id >= first).cloned().collect();
        let kept_from = requests.first().map_or(self.next_id + 1, |request| request.id);
        Some(Recording { requests, dropped: kept_from.saturating_sub(first) })
    }

    /// Ends the HAR recording; false when none was running.
    pub fn stop_har(&mut self) -> bool {
        self.har.take().is_some()
    }

    fn problem(&mut self, id: u64) {
        self.seq += 1;
        self.problems.push_back(Problem { seq: self.seq, id, at: Instant::now() });
        if self.problems.len() > CAPACITY {
            self.problems.pop_front();
        }
    }
}

impl Request {
    /// `#12 POST /api/save → 500 (fetch, 120 ms, 1.2 KB)`.
    pub fn summary(&self) -> String {
        let outcome = match (&self.failure, self.status) {
            (Some(failure), _) => failure.clone(),
            (None, Some(status)) => status.to_string(),
            (None, None) => "pending".to_owned(),
        };
        let url = output::short_url(&self.url, 100);
        let mut line = format!("#{} {} {url} → {outcome} ({}", self.id, self.method, self.kind.to_lowercase());
        if let Some(ms) = self.millis() {
            let _ = write!(line, ", {}", output::millis(ms));
        }
        if self.from_cache {
            line.push_str(", cached");
        } else if let Some(size) = self.size {
            let _ = write!(line, ", {}", output::bytes(size));
        }
        line.push(')');
        line
    }

    /// Milliseconds from issue to the last byte or the failure.
    pub fn millis(&self) -> Option<f64> {
        Some((self.ended? - self.issued) * 1000.0).filter(|ms| *ms >= 0.0)
    }

    /// Network failure or HTTP error status.
    pub fn failed(&self) -> bool {
        self.failure.is_some() || self.status.is_some_and(|status| status >= 400)
    }

    fn record(&mut self, response: Response) {
        self.status = Some(response.status);
        self.status_text = response.status_text;
        self.mime = response.mime_type.filter(|mime| !mime.is_empty());
        self.protocol = response.protocol.filter(|protocol| !protocol.is_empty());
        self.remote = response.remote_ip_address.filter(|ip| !ip.is_empty()).map(|ip| match response.remote_port {
            Some(port) if ip.contains(':') => format!("[{ip}]:{port}"),
            Some(port) => format!("{ip}:{port}"),
            None => ip,
        });
        self.from_cache |= response.from_disk_cache || response.from_prefetch_cache || response.from_service_worker;
        self.response_headers = response.headers;
        self.timing = response.timing;
    }
}

/// Resource types whose failure breaks the page rather than a detail of it.
fn breaks_page(kind: &str) -> bool {
    matches!(kind, "Document" | "XHR" | "Fetch")
}

/// Records `Network.*` events of the tab's sessions; `Fetch.*` events go on to [`crate::routes`].
pub fn on_event(page: &Page, event: &Event) {
    match event.method.as_str() {
        "Network.requestWillBeSent" => will_be_sent(page, event),
        "Network.responseReceived" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Received {
                request_id: String,
                response: Response,
            }
            if let Ok(received) = event.parse::<Received>() {
                update(page, &received.request_id, |request| {
                    request.record(received.response);
                });
            }
        }
        "Network.dataReceived" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Data {
                request_id: String,
                data_length: u64,
            }
            if let Ok(data) = event.parse::<Data>() {
                update(page, &data.request_id, |request| {
                    request.body_size += data.data_length;
                });
            }
        }
        "Network.requestServedFromCache" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Served {
                request_id: String,
            }
            if let Ok(served) = event.parse::<Served>() {
                update(page, &served.request_id, |request| {
                    request.from_cache = true;
                });
            }
        }
        "Network.loadingFinished" => loading_finished(page, event),
        "Network.loadingFailed" => loading_failed(page, event),
        _ => crate::routes::on_event(page, event),
    }
}

fn update(page: &Page, network_id: &str, change: impl FnOnce(&mut Request)) {
    if let Some(request) = page.network.lock().expect("network lock").entry(network_id) {
        change(request);
    }
}

fn will_be_sent(page: &Page, event: &Event) {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Sent {
        request_id: String,
        #[serde(default)]
        loader_id: String,
        #[serde(default)]
        frame_id: Option<String>,
        request: Http,
        timestamp: f64,
        #[serde(default)]
        wall_time: f64,
        #[serde(default)]
        initiator: Option<Initiator>,
        #[serde(rename = "type", default)]
        kind: Option<String>,
        #[serde(default)]
        redirect_response: Option<Response>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Http {
        url: String,
        method: String,
        #[serde(default)]
        headers: Value,
        #[serde(default)]
        post_data: Option<String>,
    }
    let (Ok(sent), Some(session)) = (event.parse::<Sent>(), event.session.clone()) else {
        return;
    };
    // A navigation request's id is its loader id; a redirect of the main frame's one changes
    // where the page is going before anything commits.
    if sent.redirect_response.is_some()
        && sent.request_id == sent.loader_id
        && sent.kind.as_deref() == Some("Document")
        && let Some(frame) = &sent.frame_id
    {
        page.on_document_redirect(frame, &sent.request.url);
    }
    let mut log = page.network.lock().expect("network lock");
    if let Some(redirect) = sent.redirect_response
        && let Some(previous) = log.entry(&sent.request_id)
    {
        previous.record(redirect);
        previous.done = true;
        previous.ended = Some(sent.timestamp);
    }
    log.next_id += 1;
    let request = Request {
        id: log.next_id,
        network_id: sent.request_id,
        session,
        method: sent.request.method,
        url: sent.request.url,
        kind: sent.kind.unwrap_or_else(|| "Other".to_owned()),
        initiator: sent.initiator.map(Initiator::describe).unwrap_or_default(),
        headers: sent.request.headers,
        post_data: sent.request.post_data,
        status: None,
        status_text: String::new(),
        protocol: None,
        remote: None,
        mime: None,
        response_headers: Value::Null,
        timing: None,
        failure: None,
        from_cache: false,
        size: None,
        body_size: 0,
        wall_time: sent.wall_time,
        issued: sent.timestamp,
        ended: None,
        started: Instant::now(),
        done: false,
    };
    log.push(request);
}

fn loading_finished(page: &Page, event: &Event) {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Finished {
        request_id: String,
        timestamp: f64,
        #[serde(default)]
        encoded_data_length: f64,
    }
    let Ok(finished) = event.parse::<Finished>() else {
        return;
    };
    let mut log = page.network.lock().expect("network lock");
    let Some(request) = log.entry(&finished.request_id) else {
        return;
    };
    request.done = true;
    request.ended = Some(finished.timestamp);
    request.size = Some(finished.encoded_data_length as u64);
    if request.status.is_some_and(|status| status >= 400) && breaks_page(&request.kind) {
        let id = request.id;
        log.problem(id);
    }
}

fn loading_failed(page: &Page, event: &Event) {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Failed {
        request_id: String,
        timestamp: f64,
        error_text: String,
        #[serde(default)]
        canceled: bool,
        #[serde(default)]
        blocked_reason: Option<String>,
    }
    let Ok(failed) = event.parse::<Failed>() else {
        return;
    };
    // hba's own block rules fail requests the way a content blocker does; only the rules tell
    // them apart.
    let by_client = failed.error_text == BLOCKED_BY_CLIENT;
    let url = if by_client {
        page.network.lock().expect("network lock").entry(&failed.request_id).map(|request| request.url.clone())
    } else {
        None
    };
    let intercepted = url.is_some_and(|url| {
        let rules = page.routes.lock().expect("routes lock");
        rules.find(&url).is_some_and(|rule| matches!(rule.action, crate::routes::Action::Block))
    });
    let mut log = page.network.lock().expect("network lock");
    let Some(request) = log.entry(&failed.request_id) else {
        return;
    };
    request.done = true;
    request.ended = Some(failed.timestamp);
    let (failure, problem) =
        describe_failure(failed.error_text, failed.blocked_reason.as_deref(), failed.canceled, intercepted);
    request.failure = Some(failure);
    if problem {
        let id = request.id;
        log.problem(id);
    }
}

/// Chromium's error for a request a client stopped: an extension, or Fetch interception.
const BLOCKED_BY_CLIENT: &str = "net::ERR_BLOCKED_BY_CLIENT";

/// What a failed request shows, and whether the failure is a problem of the page. A content
/// blocker (uBlock Origin ships with Helium) stops requests on purpose, so those are not.
fn describe_failure(
    error_text: String,
    blocked_reason: Option<&str>,
    canceled: bool,
    intercepted: bool,
) -> (String, bool) {
    let by_extension = error_text == BLOCKED_BY_CLIENT && !intercepted;
    let failure = match blocked_reason {
        _ if intercepted => "blocked by interception".to_owned(),
        _ if by_extension => "blocked by an extension".to_owned(),
        // The DevTools request blocking.
        Some("inspector") => "blocked by interception".to_owned(),
        Some(reason) => format!("blocked: {reason}"),
        None => error_text,
    };
    (failure, !canceled && !by_extension)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    status: u16,
    #[serde(default)]
    status_text: String,
    #[serde(default)]
    headers: Value,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(rename = "remoteIPAddress", default)]
    remote_ip_address: Option<String>,
    #[serde(default)]
    remote_port: Option<u16>,
    #[serde(default)]
    from_disk_cache: bool,
    #[serde(default)]
    from_prefetch_cache: bool,
    #[serde(default)]
    from_service_worker: bool,
    #[serde(default)]
    timing: Option<Timing>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Initiator {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    line_number: Option<f64>,
    #[serde(default)]
    stack: Option<Stack>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stack {
    #[serde(default)]
    call_frames: Vec<CallFrame>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallFrame {
    url: String,
    line_number: i64,
}

impl Initiator {
    /// `script app.js:34`, `parser index.html:12` or just the type.
    fn describe(self) -> String {
        let top = self.stack.and_then(|stack| stack.call_frames.into_iter().find(|frame| !frame.url.is_empty()));
        let place = match (top, self.url) {
            (Some(frame), _) => Some((frame.url, frame.line_number)),
            (None, Some(url)) if !url.is_empty() => Some((url, self.line_number.map_or(-1, |line| line as i64))),
            _ => None,
        };
        match place {
            Some((url, line)) if line >= 0 => {
                format!("{} {}:{}", self.kind, crate::console::short_source(&url), line + 1)
            }
            Some((url, _)) => format!("{} {}", self.kind, crate::console::short_source(&url)),
            None => self.kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u64, network_id: &str) -> Request {
        Request {
            id,
            network_id: network_id.to_owned(),
            session: Arc::from("s"),
            method: "GET".to_owned(),
            url: format!("https://example.com/{id}"),
            kind: "Fetch".to_owned(),
            initiator: String::new(),
            headers: Value::Null,
            post_data: None,
            status: None,
            status_text: String::new(),
            protocol: None,
            remote: None,
            mime: None,
            response_headers: Value::Null,
            timing: None,
            failure: None,
            from_cache: false,
            size: None,
            body_size: 0,
            wall_time: 0.0,
            issued: 10.0,
            ended: None,
            started: Instant::now(),
            done: false,
        }
    }

    fn push(log: &mut Log, network_id: &str) -> u64 {
        log.next_id += 1;
        let id = log.next_id;
        log.push(request(id, network_id));
        id
    }

    #[test]
    fn summaries_show_outcome_type_time_and_size() {
        let mut done = request(12, "r");
        done.method = "POST".to_owned();
        done.url = "https://example.com/api/save".to_owned();
        done.status = Some(500);
        done.ended = Some(10.120);
        done.size = Some(1234);
        assert_eq!(done.summary(), "#12 POST example.com/api/save → 500 (fetch, 120 ms, 1.2 KB)");
        let pending = request(3, "p");
        assert_eq!(pending.summary(), "#3 GET example.com/3 → pending (fetch)");
        let mut cached = request(4, "c");
        cached.status = Some(200);
        cached.from_cache = true;
        cached.ended = Some(10.002);
        cached.size = Some(0);
        assert_eq!(cached.summary(), "#4 GET example.com/4 → 200 (fetch, 2 ms, cached)");
    }

    #[test]
    fn ring_buffer_keeps_ids_addressable_after_eviction() {
        let mut log = Log::default();
        for n in 0..CAPACITY + 5 {
            push(&mut log, &format!("r{n}"));
        }
        assert!(log.get(5).is_none());
        assert_eq!(log.get(6).map(|r| r.network_id.as_str()), Some("r5"));
        let last = u64::try_from(CAPACITY).expect("capacity fits") + 5;
        assert_eq!(log.get(last).map(|r| r.network_id.as_str()), Some(&*format!("r{}", CAPACITY + 4)));
        assert!(log.entry("r0").is_none());
        assert!(log.entry("r6").is_some());
    }

    #[test]
    fn evicting_a_redirect_hop_keeps_the_newer_hop_reachable() {
        let mut log = Log::default();
        push(&mut log, "chain");
        for n in 0..CAPACITY - 1 {
            push(&mut log, &format!("r{n}"));
        }
        let newer = push(&mut log, "chain");
        assert_eq!(log.entry("chain").map(|r| r.id), Some(newer));
    }

    #[test]
    fn har_recording_reports_dropped_requests() {
        let mut log = Log::default();
        push(&mut log, "before");
        assert!(log.start_har());
        assert!(!log.start_har());
        push(&mut log, "a");
        push(&mut log, "b");
        let recording = log.har_recording().expect("recording");
        assert_eq!(recording.requests.iter().map(|r| r.network_id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(recording.dropped, 0);
        assert!(log.stop_har());
        assert!(log.har_recording().is_none());
        assert!(!log.stop_har());

        assert!(log.start_har());
        push(&mut log, "c");
        log.clear();
        push(&mut log, "d");
        let recording = log.har_recording().expect("recording");
        assert_eq!(recording.requests.len(), 1);
        assert_eq!(recording.dropped, 1);
    }

    #[test]
    fn blocked_requests_name_who_blocked_them() {
        let blocked = || BLOCKED_BY_CLIENT.to_owned();
        assert_eq!(describe_failure(blocked(), None, false, true), ("blocked by interception".to_owned(), true));
        assert_eq!(describe_failure(blocked(), None, false, false), ("blocked by an extension".to_owned(), false));
        let refused = describe_failure("net::ERR_CONNECTION_REFUSED".to_owned(), None, false, false);
        assert_eq!(refused, ("net::ERR_CONNECTION_REFUSED".to_owned(), true));
        assert!(!describe_failure("net::ERR_ABORTED".to_owned(), None, true, false).1);
    }

    #[test]
    fn abandoned_documents_with_content_are_downloads() {
        let since = Instant::now();
        let mut log = Log::default();
        push(&mut log, "nav");
        let request = log.entry("nav").expect("pushed");
        request.kind = "Document".to_owned();
        request.status = Some(204);
        request.failure = Some("net::ERR_ABORTED".to_owned());
        assert!(!log.turned_into_download(since), "a 204 is not a download");
        log.entry("nav").expect("pushed").status = Some(200);
        assert!(log.turned_into_download(since));
        let later = Instant::now() + Duration::from_secs(1);
        assert!(!log.turned_into_download(later), "only requests after `since` count");
    }

    #[test]
    fn initiators_name_their_source() {
        let parsed: Initiator = serde_json::from_value(serde_json::json!({
            "type": "script",
            "stack": {"callFrames": [{"url": "", "lineNumber": 0}, {"url": "https://x.test/js/app.js?v=2", "lineNumber": 33}]}
        }))
        .expect("initiator");
        assert_eq!(parsed.describe(), "script app.js:34");
        let parser: Initiator =
            serde_json::from_value(serde_json::json!({"type": "parser", "url": "https://x.test/", "lineNumber": 4}))
                .expect("initiator");
        assert_eq!(parser.describe(), "parser x.test:5");
        let other: Initiator = serde_json::from_value(serde_json::json!({"type": "other"})).expect("initiator");
        assert_eq!(other.describe(), "other");
    }
}
