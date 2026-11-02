//! `hba net`: the tab's request log, response bodies, blocking and mocking, and HAR files.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use futures_util::future::join_all;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::har::{self, Phases};
use crate::cli::{NetArgs, NetOp, StartStop};
use crate::daemon::Cx;
use crate::network::Request;
use crate::output;
use crate::page::Page;
use crate::routes::{Action, Mock, Rule};

/// Resource types `--type` accepts: the protocol's, lowercased.
const KINDS: [&str; 18] = [
    "document",
    "stylesheet",
    "image",
    "media",
    "font",
    "script",
    "texttrack",
    "xhr",
    "fetch",
    "prefetch",
    "eventsource",
    "websocket",
    "manifest",
    "signedexchange",
    "ping",
    "cspviolationreport",
    "preflight",
    "other",
];

/// Characters printed before the rest of a listing goes to a file.
const BUDGET: usize = 32_000;

/// Largest response body a HAR file includes.
const HAR_BODY_LIMIT: usize = 1 << 20;

/// Response bodies fetched concurrently while saving a HAR file.
const BODY_FETCHES: usize = 16;

/// Characters of a header value shown by `hba net show`.
const HEADER_VALUE: usize = 200;

/// Characters of request post data shown by `hba net show`.
const POST_DATA: usize = 500;

/// Headers carrying credentials: `hba net show` prints their shape, not their secrets.
const CREDENTIAL_HEADERS: [&str; 4] = ["authorization", "proxy-authorization", "cookie", "set-cookie"];

/// `hba net`: the request list, or one of its subcommands.
pub async fn net(cx: &mut Cx<'_>, args: NetArgs) -> Result<()> {
    let page = cx.page().await?;
    let NetArgs { op, filter, kind, failed, last } = args;
    match op {
        None => list(cx, &page, &Filter { text: filter, kind, failed }, last),
        Some(NetOp::Show { id }) => show(cx, &page, &id),
        Some(NetOp::Body { id, output, max }) => body(cx, &page, &id, output.as_deref(), max).await,
        Some(NetOp::Block { patterns }) => {
            let rules = patterns
                .into_iter()
                .map(|pattern| check_pattern(&pattern).map(|()| Rule { pattern, action: Action::Block }))
                .collect::<Result<Vec<Rule>>>()?;
            route(cx, &page, rules).await
        }
        Some(NetOp::Mock { pattern, status, body, file, content_type, headers }) => {
            check_pattern(&pattern)?;
            let content = match (body, file) {
                (Some(text), _) => Content { bytes: text.into_bytes(), file_type: None },
                (None, Some(file)) => {
                    let path = cx.path(&file);
                    let bytes = tokio::fs::read(&path).await.with_context(|| format!("reading {}", path.display()))?;
                    Content { bytes, file_type: type_of_path(&path) }
                }
                (None, None) => Content { bytes: Vec::new(), file_type: None },
            };
            let mock = mock(status, &content, content_type, &headers)?;
            route(cx, &page, vec![Rule { pattern, action: Action::Mock(Arc::new(mock)) }]).await
        }
        Some(NetOp::Unroute { pattern }) => unroute(cx, &page, pattern.as_deref()).await,
        Some(NetOp::Har { action: StartStop::Start, path }) => har_start(cx, &page, path.as_deref()),
        Some(NetOp::Har { action: StartStop::Stop, path }) => har_stop(cx, &page, path.as_deref()).await,
        Some(NetOp::Clear) => {
            let cleared = {
                let mut log = page.network.lock().expect("network lock");
                let count = log.iter().count();
                log.clear();
                count
            };
            cx.say(format!("cleared {}", output::count(cleared, "request")));
            Ok(())
        }
    }
}

/// What `hba net` lists.
struct Filter {
    text: Option<String>,
    kind: Option<String>,
    failed: bool,
}

impl Filter {
    fn accepts(&self, request: &Request) -> bool {
        self.text.as_deref().is_none_or(|text| request.url.contains(text))
            && self.kind.as_deref().is_none_or(|kind| request.kind.eq_ignore_ascii_case(kind))
            && (!self.failed || request.failed())
    }
}

fn list(cx: &mut Cx<'_>, page: &Page, filter: &Filter, last: usize) -> Result<()> {
    if let Some(kind) = &filter.kind
        && !KINDS.contains(&kind.to_lowercase().as_str())
    {
        bail!("unknown --type {kind}; use one of: {}", KINDS.join(", "));
    }
    let rules: Vec<String> =
        page.routes.lock().expect("routes lock").iter().map(|rule| format!("rule {rule}")).collect();
    let (lines, json, earlier, total) = {
        let log = page.network.lock().expect("network lock");
        let matching: Vec<&Request> = log.iter().filter(|request| filter.accepts(request)).collect();
        let earlier = matching.len().saturating_sub(last);
        let shown = &matching[earlier..];
        let json: Vec<Value> =
            if cx.json { shown.iter().map(|request| request_json(request)).collect() } else { Vec::new() };
        (shown.iter().map(|request| request.summary()).collect::<Vec<String>>(), json, earlier, log.iter().count())
    };
    if cx.json {
        cx.say(serde_json::to_string(&json)?);
        return Ok(());
    }
    let mut text = String::new();
    for rule in &rules {
        let _ = writeln!(text, "{rule}");
    }
    if earlier > 0 {
        let _ = writeln!(text, "… {} (raise -n)", output::count(earlier, "earlier request"));
    }
    for line in &lines {
        let _ = writeln!(text, "{line}");
    }
    if lines.is_empty() && earlier == 0 {
        text.push_str(if total == 0 { "no requests since hba attached to this tab\n" } else { "no requests match\n" });
    }
    crate::inspect::emit_budgeted(cx, &text, BUDGET, "network", "log", "use -f, --type, --failed or -n to narrow it")
}

fn show(cx: &mut Cx<'_>, page: &Page, id: &str) -> Result<()> {
    let request = lookup(page, id)?;
    if cx.json {
        let mut json = request_json(&request);
        json["requestHeaders"] = Value::Array(header_lines(&request.headers).into_iter().map(Value::from).collect());
        json["responseHeaders"] =
            Value::Array(header_lines(&request.response_headers).into_iter().map(Value::from).collect());
        json["postData"] = json!(request.post_data);
        cx.say(serde_json::to_string(&json)?);
        return Ok(());
    }
    cx.say(describe(&request));
    Ok(())
}

/// Everything `hba net show` prints about a request.
fn describe(request: &Request) -> String {
    let mut out = String::new();
    let outcome = match (&request.failure, request.status) {
        (Some(failure), _) => failure.clone(),
        (None, Some(status)) => format!("{status} {}", request.status_text).trim_end().to_owned(),
        (None, None) => "pending".to_owned(),
    };
    let _ = writeln!(out, "#{} {} {} → {outcome}", request.id, request.method, request.url);
    let mut facts = vec![request.kind.to_lowercase()];
    facts.extend(request.protocol.clone());
    facts.extend(request.remote.clone());
    if request.from_cache {
        facts.push("from cache".to_owned());
    }
    if !request.initiator.is_empty() {
        facts.push(format!("initiator {}", request.initiator));
    }
    let _ = writeln!(out, "{}", facts.join(", "));
    if request.status.is_some() {
        let mut sizes = vec![request.mime.clone().unwrap_or_else(|| "no content type".to_owned())];
        if let Some(size) = request.size {
            sizes.push(format!("{} transferred", output::bytes(size)));
        }
        if request.body_size > 0 {
            sizes.push(format!("{} body", output::bytes(request.body_size)));
        }
        let _ = writeln!(out, "{}", sizes.join(", "));
    }
    if request.ended.is_some() {
        let _ = writeln!(out, "timing {}", timing_line(&Phases::of(request)));
    }
    for line in header_lines(&request.headers) {
        let _ = writeln!(out, "> {line}");
    }
    for line in header_lines(&request.response_headers) {
        let _ = writeln!(out, "< {line}");
    }
    if let Some(data) = &request.post_data {
        let _ =
            writeln!(out, "post data ({}): {}", output::bytes(data.len() as u64), output::truncate(data, POST_DATA));
    }
    out
}

/// `total 120 ms: blocked 1 ms, dns 3 ms, connect 20 ms (ssl 12 ms), send 0.1 ms, wait 80 ms, receive 4 ms`.
fn timing_line(phases: &Phases) -> String {
    let mut parts = Vec::new();
    for (name, ms) in [("blocked", phases.blocked), ("dns", phases.dns), ("connect", phases.connect)] {
        if ms >= 0.0 {
            parts.push(format!("{name} {}", output::millis(ms)));
        }
    }
    if phases.ssl >= 0.0 && phases.connect >= 0.0 {
        let connect = parts.pop().unwrap_or_default();
        parts.push(format!("{connect} (ssl {})", output::millis(phases.ssl)));
    }
    for (name, ms) in [("send", phases.send), ("wait", phases.wait), ("receive", phases.receive)] {
        parts.push(format!("{name} {}", output::millis(ms)));
    }
    format!("total {}: {}", output::millis(phases.total()), parts.join(", "))
}

/// `name: value` lines sorted by name, values cut short and credentials reduced to their shape.
fn header_lines(headers: &Value) -> Vec<String> {
    let Some(map) = headers.as_object() else {
        return Vec::new();
    };
    let mut named: Vec<(String, String)> = Vec::with_capacity(map.len());
    for (name, value) in map {
        let lower = name.to_ascii_lowercase();
        for value in value.as_str().unwrap_or_default().split('\n') {
            let shown = if CREDENTIAL_HEADERS.contains(&lower.as_str()) {
                redact(&lower, value)
            } else {
                output::truncate(value, HEADER_VALUE).into_owned()
            };
            named.push((lower.clone(), shown));
        }
    }
    // Stable, so repeated headers keep their order.
    named.sort_by(|a, b| a.0.cmp(&b.0));
    named.into_iter().map(|(name, value)| format!("{name}: {value}")).collect()
}

/// Keeps what helps debugging (scheme, cookie names, attributes) and hides the secret.
fn redact(name: &str, value: &str) -> String {
    let hidden = |secret: &str| format!("…({} chars)", secret.chars().count());
    match name {
        "cookie" => value
            .split(';')
            .map(|pair| {
                pair.trim()
                    .split_once('=')
                    .map_or_else(|| pair.trim().to_owned(), |(name, secret)| format!("{name}={}", hidden(secret)))
            })
            .collect::<Vec<_>>()
            .join("; "),
        "set-cookie" => {
            let (pair, attributes) = value.split_once(';').unwrap_or((value, ""));
            let pair = pair
                .split_once('=')
                .map_or_else(|| pair.to_owned(), |(name, secret)| format!("{name}={}", hidden(secret)));
            if attributes.is_empty() { pair } else { format!("{pair};{attributes}") }
        }
        _ => match value.split_once(' ') {
            Some((scheme, secret)) => format!("{scheme} {}", hidden(secret)),
            None => hidden(value),
        },
    }
}

async fn fetch_body(page: &Page, request: &Request, timeout: Duration) -> Result<har::Body> {
    let raw = page
        .cdp
        .raw_within(
            "Network.getResponseBody",
            json!({"requestId": request.network_id}),
            Some(&request.session),
            timeout,
        )
        .await
        .map_err(|error| {
            // The browser answers with an error once it dropped the body; a timeout is just slow.
            if error.is::<crate::cdp::ProtocolError>() {
                error.context(format!(
                    "the body of #{} is no longer available (the browser keeps recent bodies only)",
                    request.id
                ))
            } else {
                error
            }
        })?;
    serde_json::from_str(raw.get()).context("decoding the response body")
}

/// Whether a status is a redirect, whose body is never kept. A 304 is answered from the cache.
fn is_redirect(status: u16) -> bool {
    (300..400).contains(&status) && status != 304
}

async fn body(cx: &mut Cx<'_>, page: &Page, id: &str, output: Option<&Path>, max: usize) -> Result<()> {
    let request = lookup(page, id)?;
    if let Some(failure) = &request.failure {
        bail!("#{} failed ({failure}) and has no body", request.id);
    }
    if !request.done {
        bail!("#{} is still loading; retry once `hba net` shows its size", request.id);
    }
    if let Some(status) = request.status.filter(|status| is_redirect(*status)) {
        bail!("#{} is a {status} redirect; its target is a later request in `hba net`", request.id);
    }
    let fetched = fetch_body(page, &request, cx.timeout).await?;
    let bytes = if fetched.base64 {
        base64::engine::general_purpose::STANDARD.decode(fetched.text.as_bytes()).context("decoding the body")?
    } else {
        fetched.text.into_bytes()
    };
    let mime = request.mime.as_deref().unwrap_or("unknown type");
    if let Some(output) = output {
        let path = cx.path(output);
        output::write_private(&path, &bytes)?;
        cx.say(format!("saved {} ({mime}, {})", path.display(), output::bytes(bytes.len() as u64)));
        return Ok(());
    }
    let size = bytes.len() as u64;
    let Ok(text) = String::from_utf8(bytes) else {
        bail!(
            "#{} has a binary body ({mime}, {}); save it with `hba net body {} -o FILE`",
            request.id,
            output::bytes(size),
            request.id
        );
    };
    if text.is_empty() {
        cx.say(format!("#{} has an empty body", request.id));
        return Ok(());
    }
    crate::inspect::emit_budgeted(cx, &text, max, "body", extension_of(mime), "the whole body is in the file")
}

/// File extension for a body's MIME type.
fn extension_of(mime: &str) -> &'static str {
    let essence = mime.split(';').next().unwrap_or(mime).trim();
    match essence {
        "text/html" => "html",
        "text/css" => "css",
        "image/svg+xml" => "svg",
        _ if essence.contains("json") => "json",
        _ if essence.contains("javascript") || essence.contains("ecmascript") => "js",
        _ if essence.contains("xml") => "xml",
        _ => "txt",
    }
}

/// A request by `12` or `#12`.
fn lookup(page: &Page, id: &str) -> Result<Request> {
    let number: u64 = id
        .trim_start_matches('#')
        .parse()
        .with_context(|| format!("{id} is not a request id; `hba net` lists them"))?;
    page.network.lock().expect("network lock").get(number).cloned().with_context(|| {
        format!("request #{number} is not in the log (it keeps the latest 1000); `hba net` lists them")
    })
}

pub(super) fn request_json(request: &Request) -> Value {
    json!({
        "id": request.id,
        "method": request.method,
        "url": request.url,
        "type": request.kind.to_lowercase(),
        "status": request.status,
        "failure": request.failure,
        "mime": request.mime,
        "ms": request.millis().map(f64::round),
        "size": request.size,
        "fromCache": request.from_cache,
    })
}

/// Rejects a pattern that could only match one exact URL by accident, like `missing.json`.
fn check_pattern(pattern: &str) -> Result<()> {
    if pattern.is_empty() {
        bail!("the URL pattern is empty");
    }
    if !pattern.contains(['*', '?']) && !pattern.contains("://") {
        bail!("{pattern} must match the whole URL; did you mean *{pattern}*?");
    }
    Ok(())
}

/// A mock's body and the content type its file name suggests.
struct Content {
    bytes: Vec<u8>,
    file_type: Option<&'static str>,
}

fn mock(status: u16, content: &Content, content_type: Option<String>, extra: &[String]) -> Result<Mock> {
    if !(100..=599).contains(&status) {
        bail!("--status {status} is not an HTTP status code");
    }
    let content_type =
        content_type.unwrap_or_else(|| content.file_type.unwrap_or_else(|| sniff(&content.bytes)).to_owned());
    let mut headers = vec![("Content-Type".to_owned(), content_type)];
    for header in extra {
        let (name, value) =
            header.split_once(':').with_context(|| format!("--header {header:?} needs the form Name:Value"))?;
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() {
            bail!("--header {header:?} has no name");
        }
        if name.eq_ignore_ascii_case("content-type") {
            value.clone_into(&mut headers[0].1);
        } else {
            headers.push((name.to_owned(), value.to_owned()));
        }
    }
    Ok(Mock::new(status, headers, &content.bytes))
}

/// Content type of a mock body read from `path`, by extension.
fn type_of_path(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "json" => "application/json",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        _ => return None,
    })
}

/// Content type of an inline mock body: JSON, HTML, text or bytes.
fn sniff(body: &[u8]) -> &'static str {
    if !body.is_empty() && serde_json::from_slice::<serde::de::IgnoredAny>(body).is_ok() {
        return "application/json";
    }
    match std::str::from_utf8(body) {
        Ok(text) if text.trim_start().starts_with('<') => "text/html; charset=utf-8",
        Ok(_) => "text/plain; charset=utf-8",
        Err(_) => "application/octet-stream",
    }
}

async fn route(cx: &mut Cx<'_>, page: &Page, rules: Vec<Rule>) -> Result<()> {
    let added: Vec<String> = rules.iter().map(ToString::to_string).collect();
    let total = {
        let mut table = page.routes.lock().expect("routes lock");
        for rule in rules {
            table.add(rule);
        }
        table.len()
    };
    crate::routes::apply(page).await?;
    let tab = cx.tab_number(&page.tab);
    for rule in &added {
        cx.say(format!("{rule} on {tab}"));
    }
    cx.say(format!("{} active; `hba net unroute [PATTERN]` removes them", output::count(total, "rule")));
    Ok(())
}

async fn unroute(cx: &mut Cx<'_>, page: &Page, pattern: Option<&str>) -> Result<()> {
    let (removed, left) = {
        let mut rules = page.routes.lock().expect("routes lock");
        let removed = rules.remove(pattern);
        (removed, rules.iter().map(ToString::to_string).collect::<Vec<String>>())
    };
    if removed == 0 {
        match pattern {
            Some(pattern) if left.is_empty() => bail!("no rule for {pattern}; no rules are active"),
            Some(pattern) => bail!("no rule for {pattern}; active: {}", left.join("; ")),
            None => {
                cx.say("no rules were active");
                return Ok(());
            }
        }
    }
    crate::routes::apply(page).await?;
    if left.is_empty() {
        cx.say(format!("removed {}; requests are no longer intercepted", output::count(removed, "rule")));
    } else {
        cx.say(format!("removed {}; still active:", output::count(removed, "rule")));
        for rule in left {
            cx.say(format!("rule {rule}"));
        }
    }
    Ok(())
}

fn har_start(cx: &mut Cx<'_>, page: &Page, path: Option<&Path>) -> Result<()> {
    if let Some(path) = path {
        bail!("the file goes with stop: `hba net har stop {}`", path.display());
    }
    if !page.network.lock().expect("network lock").start_har() {
        bail!("a HAR recording is already running; `hba net har stop FILE` saves it");
    }
    cx.say(format!("recording HAR on {}; `hba net har stop FILE` saves it", cx.tab_number(&page.tab)));
    Ok(())
}

async fn har_stop(cx: &mut Cx<'_>, page: &Page, path: Option<&Path>) -> Result<()> {
    let recording = page
        .network
        .lock()
        .expect("network lock")
        .har_recording()
        .context("no HAR recording is running; `hba net har start` begins one")?;
    let path = match path {
        Some(path) => cx.path(path),
        None => cx.artifact("network", "har")?,
    };
    // Bodies come from the page's renderer; a busy page answers slowly, so they get the
    // command's time and the file is saved with what arrived.
    let deadline = Instant::now() + cx.timeout;
    let mut bodies: Vec<Option<har::Body>> = vec![None; recording.requests.len()];
    let wanted: Vec<usize> = (0..recording.requests.len()).filter(|&i| wants_body(&recording.requests[i])).collect();
    let mut missed = 0;
    for batch in wanted.chunks(BODY_FETCHES) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            missed += batch.len();
            continue;
        }
        let fetched = join_all(batch.iter().map(|&i| fetch_body(page, &recording.requests[i], left))).await;
        let late = Instant::now() >= deadline;
        for (&i, result) in batch.iter().zip(fetched) {
            match result {
                Ok(body) => bodies[i] = (body.decoded_len() <= HAR_BODY_LIMIT).then_some(body),
                Err(_) if late => missed += 1,
                // Helium no longer holds it.
                Err(_) => {}
            }
        }
    }
    let document = har::build(&recording.requests, &bodies, env!("CARGO_PKG_VERSION"));
    let bytes = serde_json::to_vec(&document)?;
    output::write_private(&path, &bytes)?;
    // Ended only once saved: a failed write or a hang-up keeps it for another `stop`.
    page.network.lock().expect("network lock").stop_har();
    let with_bodies = bodies.iter().filter(|body| body.is_some()).count();
    cx.say(format!(
        "saved {} ({} requests, {with_bodies} with bodies, {})",
        path.display(),
        recording.requests.len(),
        output::bytes(bytes.len() as u64)
    ));
    if recording.dropped > 0 {
        cx.say(format!("{} recorded requests had left the log (it keeps the latest 1000)", recording.dropped));
    }
    if missed > 0 {
        cx.say(format!(
            "{missed} response {} left out: the page did not hand {} over within {:.0}s (raise --timeout)",
            if missed == 1 { "body is" } else { "bodies are" },
            if missed == 1 { "it" } else { "them" },
            cx.timeout.as_secs_f32()
        ));
    }
    Ok(())
}

/// Whether a HAR file includes the response body: text of a finished response up to
/// [`HAR_BODY_LIMIT`].
fn wants_body(request: &Request) -> bool {
    let text_like = request.mime.as_deref().is_some_and(|mime| {
        mime.starts_with("text/") || ["json", "javascript", "ecmascript", "xml"].iter().any(|kind| mime.contains(kind))
    });
    let has_body = request.status.is_some_and(|status| !is_redirect(status) && status != 204);
    request.done && request.failure.is_none() && has_body && text_like && request.body_size <= HAR_BODY_LIMIT as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_show_their_shape_only() {
        let headers = json!({
            "Authorization": "Bearer abc.def.ghi",
            "Cookie": "sid=s3cret; theme=dark",
            "set-cookie": "sid=s3cret; Path=/; HttpOnly\nflag=1",
            "Accept": "*/*",
        });
        let lines = header_lines(&headers);
        assert_eq!(
            lines,
            [
                "accept: */*",
                "authorization: Bearer …(11 chars)",
                "cookie: sid=…(6 chars); theme=…(4 chars)",
                "set-cookie: sid=…(6 chars); Path=/; HttpOnly",
                "set-cookie: flag=…(1 chars)",
            ]
        );
    }

    #[test]
    fn timing_lines_skip_phases_that_did_not_happen() {
        let phases = Phases { blocked: 1.0, dns: -1.0, connect: 20.0, ssl: 12.0, send: 0.2, wait: 80.0, receive: 4.0 };
        assert_eq!(
            timing_line(&phases),
            "total 105 ms: blocked 1 ms, connect 20 ms (ssl 12 ms), send 0.2 ms, wait 80 ms, receive 4 ms"
        );
    }

    #[test]
    fn patterns_must_cover_the_whole_url() {
        assert!(check_pattern("*missing.json").is_ok());
        assert!(check_pattern("https://a.test/x.json").is_ok());
        let error = check_pattern("missing.json").expect_err("bare file name");
        assert!(error.to_string().contains("*missing.json*"), "{error}");
    }

    #[test]
    fn mocks_infer_their_content_type() {
        let json = mock(200, &Content { bytes: b"{\"a\":1}".to_vec(), file_type: None }, None, &[]).expect("mock");
        assert_eq!(json.headers, [("Content-Type".to_owned(), "application/json".to_owned())]);
        let html = mock(200, &Content { bytes: b"<p>hi</p>".to_vec(), file_type: None }, None, &["X-Test: 1".into()])
            .expect("mock");
        assert_eq!(html.headers[0].1, "text/html; charset=utf-8");
        assert_eq!(html.headers[1], ("X-Test".to_owned(), "1".to_owned()));
        let typed = mock(
            404,
            &Content { bytes: Vec::new(), file_type: Some("image/png") },
            None,
            &["content-type: text/csv".into()],
        )
        .expect("mock");
        assert_eq!(typed.headers, [("Content-Type".to_owned(), "text/csv".to_owned())]);
        assert!(mock(700, &Content { bytes: Vec::new(), file_type: None }, None, &[]).is_err());
        assert!(mock(200, &Content { bytes: Vec::new(), file_type: None }, None, &["no-colon".into()]).is_err());
        assert_eq!(type_of_path(Path::new("/tmp/data.JSON")), Some("application/json"));
    }

    #[test]
    fn har_files_keep_bodies_of_cached_responses_but_not_redirects() {
        let text = |status: u16| Request { status: Some(status), mime: Some("text/html".into()), ..sample() };
        assert!(wants_body(&text(200)));
        assert!(wants_body(&text(304)), "a 304 is the cached body, not a redirect");
        assert!(!wants_body(&text(302)));
        assert!(!wants_body(&text(204)));
        assert!(!wants_body(&Request { mime: Some("image/png".into()), ..sample() }));
        assert!(!wants_body(&Request { body_size: HAR_BODY_LIMIT as u64 + 1, ..text(200) }));
    }

    #[test]
    fn filters_combine() {
        let filter = Filter { text: Some("/api/".into()), kind: Some("fetch".into()), failed: true };
        let mut request = sample();
        assert!(!filter.accepts(&request), "not failed");
        request.status = Some(500);
        assert!(filter.accepts(&request));
        request.kind = "Script".into();
        assert!(!filter.accepts(&request));
    }

    fn sample() -> Request {
        Request {
            id: 1,
            network_id: "1".into(),
            session: Arc::from("s"),
            method: "GET".into(),
            url: "https://a.test/api/items".into(),
            kind: "Fetch".into(),
            initiator: String::new(),
            headers: Value::Null,
            post_data: None,
            status: Some(200),
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
            issued: 0.0,
            ended: None,
            started: std::time::Instant::now(),
            done: true,
        }
    }
}
