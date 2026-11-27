//! HAR 1.2 documents built from the network log, and the request timing phases they share
//! with `hba net show`.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::network::Request;
use crate::output::iso8601;
use crate::routes::header;

/// A response body as `Network.getResponseBody` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Body {
    #[serde(rename = "body")]
    pub text: String,
    #[serde(rename = "base64Encoded")]
    pub base64: bool,
}

impl Body {
    /// Length of the decoded body in bytes.
    pub fn decoded_len(&self) -> usize {
        if self.base64 {
            self.text.len() / 4 * 3 - self.text.bytes().rev().take_while(|b| *b == b'=').count()
        } else {
            self.text.len()
        }
    }
}

/// Request phases in milliseconds, as HAR defines them; -1 when a phase did not happen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Phases {
    /// Queueing and stalling before the first network activity.
    pub blocked: f64,
    pub dns: f64,
    /// TCP connect, TLS included.
    pub connect: f64,
    pub ssl: f64,
    pub send: f64,
    /// Waiting for the first response byte.
    pub wait: f64,
    pub receive: f64,
}

impl Phases {
    /// The request's phases from its protocol timing.
    pub fn of(request: &Request) -> Self {
        let Some(timing) = request.timing else {
            // Cache hits, data URLs and early failures have no connection timing.
            return Self {
                blocked: -1.0,
                dns: -1.0,
                connect: -1.0,
                ssl: -1.0,
                send: 0.0,
                wait: 0.0,
                receive: request.millis().unwrap_or(0.0),
            };
        };
        let span = |start: f64, end: f64| {
            if start >= 0.0 && end >= 0.0 { (end - start).max(0.0) } else { -1.0 }
        };
        let queued = ((timing.request_time - request.issued) * 1000.0).max(0.0);
        let first_activity = [timing.dns_start, timing.connect_start, timing.send_start]
            .into_iter()
            .filter(|at| *at >= 0.0)
            .reduce(f64::min);
        let headers = timing.receive_headers_end.max(0.0);
        let end = request.ended.map_or(headers, |ended| (ended - timing.request_time) * 1000.0);
        Self {
            blocked: queued + first_activity.unwrap_or(0.0),
            dns: span(timing.dns_start, timing.dns_end),
            connect: span(timing.connect_start, timing.connect_end),
            ssl: span(timing.ssl_start, timing.ssl_end),
            send: span(timing.send_start, timing.send_end).max(0.0),
            wait: span(timing.send_end, timing.receive_headers_end).max(0.0),
            receive: (end - headers).max(0.0),
        }
    }

    /// Total time; `ssl` is part of `connect`.
    pub fn total(&self) -> f64 {
        [self.blocked, self.dns, self.connect, self.send, self.wait, self.receive]
            .into_iter()
            .filter(|ms| *ms > 0.0)
            .sum()
    }
}

/// A HAR 1.2 log of `requests`; `bodies[i]` is the response body of `requests[i]`, if fetched.
/// The bodies are moved into the document: they run to megabytes.
pub fn build(requests: &[Request], bodies: Vec<Option<Body>>, version: &str) -> Value {
    let mut bodies = bodies.into_iter();
    let entries: Vec<Value> = requests.iter().map(|request| entry(request, bodies.next().flatten())).collect();
    json!({
        "log": {
            "version": "1.2",
            "creator": {"name": "hba", "version": version},
            "entries": entries,
        }
    })
}

fn entry(request: &Request, body: Option<Body>) -> Value {
    let phases = Phases::of(request);
    let http_version = http_version(request.protocol.as_deref());
    let mut har_request = json!({
        "method": request.method,
        "url": request.url,
        "httpVersion": http_version,
        "cookies": request_cookies(&request.headers),
        "headers": headers(&request.headers),
        "queryString": query_string(&request.url),
        "headersSize": -1,
        "bodySize": request.post_data.as_ref().map_or(0, String::len),
    });
    if let Some(text) = &request.post_data {
        let mime = header(&request.headers, "content-type").unwrap_or_default();
        har_request["postData"] = json!({"mimeType": mime, "text": text});
    }
    let mut content = json!({
        "size": body.as_ref().map_or(request.body_size, |body| body.decoded_len() as u64),
        "mimeType": request.mime.as_deref().unwrap_or("x-unknown"),
    });
    if let Some(body) = body {
        content["text"] = Value::String(body.text);
        if body.base64 {
            content["encoding"] = json!("base64");
        }
    }
    let size = request.size.map_or(-1, |size| i64::try_from(size).unwrap_or(i64::MAX));
    let response = json!({
        "status": request.status.unwrap_or(0),
        "statusText": request.status_text,
        "httpVersion": http_version,
        "cookies": response_cookies(&request.response_headers, request.wall_time),
        "headers": headers(&request.response_headers),
        "content": content,
        "redirectURL": header(&request.response_headers, "location").unwrap_or_default(),
        "headersSize": -1,
        "bodySize": size,
        "_transferSize": size,
    });
    let round = |ms: f64| {
        if ms < 0.0 { -1.0 } else { (ms * 1000.0).round() / 1000.0 }
    };
    let mut entry = json!({
        "startedDateTime": iso8601(request.wall_time),
        "time": round(phases.total()),
        "request": har_request,
        "response": response,
        "cache": {},
        "timings": {
            "blocked": round(phases.blocked),
            "dns": round(phases.dns),
            "connect": round(phases.connect),
            "ssl": round(phases.ssl),
            "send": round(phases.send),
            "wait": round(phases.wait),
            "receive": round(phases.receive),
        },
        "_resourceType": request.kind.to_lowercase(),
    });
    if let Some(ip) = request.remote.as_deref().map(ip_of) {
        entry["serverIPAddress"] = json!(ip);
    }
    if let Some(failure) = &request.failure {
        entry["_error"] = json!(failure);
    }
    entry
}

/// `h2` → `HTTP/2`, `http/1.1` → `HTTP/1.1`; empty when unknown.
fn http_version(protocol: Option<&str>) -> String {
    match protocol {
        Some("h2") => "HTTP/2".to_owned(),
        Some("h3" | "h3-29") => "HTTP/3".to_owned(),
        Some(other) => other.to_uppercase(),
        None => String::new(),
    }
}

/// Header object → HAR list; Chrome joins repeated headers with newlines.
fn headers(headers: &Value) -> Vec<Value> {
    let Some(map) = headers.as_object() else {
        return Vec::new();
    };
    map.iter()
        .flat_map(|(name, value)| {
            value.as_str().unwrap_or_default().split('\n').map(move |value| json!({"name": name, "value": value}))
        })
        .collect()
}

/// The `Cookie` header as HAR cookies; a pair without `=` is a cookie without a name.
fn request_cookies(headers: &Value) -> Vec<Value> {
    let Some(cookies) = header(headers, "cookie") else {
        return Vec::new();
    };
    cookies
        .split(['\n', ';'])
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or(("", pair));
            json!({"name": name.trim(), "value": value.trim()})
        })
        .collect()
}

/// The `Set-Cookie` lines as HAR cookies. `Max-Age` counts from `now` (seconds since the epoch)
/// and wins over `Expires`, as in browsers.
fn response_cookies(headers: &Value, now: f64) -> Vec<Value> {
    let Some(lines) = header(headers, "set-cookie") else {
        return Vec::new();
    };
    lines.split('\n').filter_map(|line| set_cookie(line, now)).collect()
}

fn set_cookie(line: &str, now: f64) -> Option<Value> {
    let mut parts = line.split(';');
    let pair = parts.next()?.trim();
    if pair.is_empty() {
        return None;
    }
    let (name, value) = pair.split_once('=').unwrap_or(("", pair));
    let mut cookie = json!({"name": name.trim(), "value": value.trim(), "httpOnly": false, "secure": false});
    let (mut expires, mut max_age) = (None, None);
    for attribute in parts {
        let (key, value) = attribute.split_once('=').unwrap_or((attribute, ""));
        let (key, value) = (key.trim(), value.trim());
        if key.eq_ignore_ascii_case("path") {
            cookie["path"] = json!(value);
        } else if key.eq_ignore_ascii_case("domain") {
            cookie["domain"] = json!(value);
        } else if key.eq_ignore_ascii_case("expires") {
            expires = cookie_date(value);
        } else if key.eq_ignore_ascii_case("max-age") {
            max_age = value.parse::<i64>().ok().filter(|_| !value.starts_with('+'));
        } else if key.eq_ignore_ascii_case("httponly") {
            cookie["httpOnly"] = json!(true);
        } else if key.eq_ignore_ascii_case("secure") {
            cookie["secure"] = json!(true);
        }
    }
    if let Some(at) = max_age.map(|seconds| now + seconds as f64).or(expires) {
        cookie["expires"] = json!(iso8601(at));
    }
    Some(cookie)
}

/// A cookie date as seconds since the epoch, parsed as RFC 6265 §5.1.1 says: `Wed, 21 Oct 2015
/// 07:28:00 GMT` and the older forms servers still send.
fn cookie_date(text: &str) -> Option<f64> {
    let delimiter = |c: char| matches!(c, '\t' | ' '..='/' | ';'..='@' | '['..='`' | '{'..='~');
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);
    for token in text.split(delimiter).filter(|token| !token.is_empty()) {
        if time.is_none()
            && let Some(clock) = clock(token)
        {
            time = Some(clock);
        } else if day.is_none()
            && let Some((n, _)) = digits(token, 1, 2)
        {
            day = Some(n);
        } else if month.is_none()
            && let Some(n) = month_of(token)
        {
            month = Some(n);
        } else if year.is_none()
            && let Some((n, _)) = digits(token, 2, 4)
        {
            year = Some(match n {
                70..=99 => n + 1900,
                0..=69 => n + 2000,
                _ => n,
            });
        }
    }
    let ((hour, minute, second), day, month, year) = (time?, day?, month?, year?);
    if !(1..=31).contains(&day) || year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(i64::from(year), i64::from(month), i64::from(day));
    Some((days * 86_400 + i64::from(hour * 3600 + minute * 60 + second)) as f64)
}

/// `hh:mm:ss` at the start of a token, one or two digits each.
fn clock(token: &str) -> Option<(u32, u32, u32)> {
    let (hour, rest) = digits(token, 1, 2)?;
    let (minute, rest) = digits(rest.strip_prefix(':')?, 1, 2)?;
    let (second, _) = digits(rest.strip_prefix(':')?, 1, 2)?;
    Some((hour, minute, second))
}

/// The `min` to `max` digits a token starts with, as a number, and the rest of the token.
fn digits(token: &str, min: usize, max: usize) -> Option<(u32, &str)> {
    let end = token.find(|c: char| !c.is_ascii_digit()).unwrap_or(token.len());
    if !(min..=max).contains(&end) {
        return None;
    }
    Some((token[..end].parse().ok()?, &token[end..]))
}

/// 1 to 12 for a token starting with a month's English abbreviation.
fn month_of(token: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let prefix = token.get(..3)?;
    MONTHS.iter().position(|month| month.eq_ignore_ascii_case(prefix)).map(|index| index as u32 + 1)
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar, by Howard Hinnant's
/// days-from-civil algorithm (the inverse of the one [`iso8601`] uses).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn query_string(url: &str) -> Vec<Value> {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let Some((_, query)) = without_fragment.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            json!({"name": name, "value": value})
        })
        .collect()
}

/// `1.2.3.4:443` → `1.2.3.4`, `[::1]:8080` → `::1`.
fn ip_of(remote: &str) -> &str {
    if let Some(rest) = remote.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    remote.rsplit_once(':').map_or(remote, |(ip, _)| ip)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;
    use crate::network::Timing;

    fn request() -> Request {
        Request {
            id: 7,
            network_id: "1.7".to_owned(),
            session: Arc::from("s"),
            method: "POST".to_owned(),
            url: "https://api.test/items?page=2&q=a%20b#top".to_owned(),
            kind: "Fetch".to_owned(),
            initiator: "script app.js:3".to_owned(),
            headers: json!({"Content-Type": "application/json", "Accept": "*/*"}),
            post_data: Some("{\"n\":1}".to_owned()),
            status: Some(302),
            status_text: "Found".to_owned(),
            protocol: Some("h2".to_owned()),
            remote: Some("[2001:db8::1]:443".to_owned()),
            mime: Some("application/json".to_owned()),
            response_headers: json!({"location": "/next", "set-cookie": "a=1\nb=2"}),
            raw_response: true,
            timing: Some(Timing {
                request_time: 100.010,
                dns_start: 1.0,
                dns_end: 4.0,
                connect_start: 4.0,
                connect_end: 20.0,
                ssl_start: 8.0,
                ssl_end: 20.0,
                send_start: 21.0,
                send_end: 22.0,
                receive_headers_end: 72.0,
            }),
            failure: None,
            from_cache: false,
            size: Some(900),
            body_size: 11,
            wall_time: 1_700_000_000.5,
            issued: 100.0,
            ended: Some(100.090),
            started: Instant::now(),
            done: true,
        }
    }

    #[test]
    fn phases_add_up_to_the_request_time() {
        let phases = Phases::of(&request());
        assert_eq!(phases.blocked.round(), 11.0, "10 ms queued + 1 ms before DNS");
        assert_eq!((phases.dns, phases.connect, phases.ssl, phases.send), (3.0, 16.0, 12.0, 1.0));
        assert_eq!(phases.wait, 50.0);
        assert_eq!(phases.receive.round(), 8.0);
        assert_eq!(phases.total().round(), 89.0);
    }

    #[test]
    fn requests_without_timing_spend_their_time_receiving() {
        let mut cached = request();
        cached.timing = None;
        let phases = Phases::of(&cached);
        assert_eq!((phases.blocked, phases.dns, phases.connect), (-1.0, -1.0, -1.0));
        assert_eq!(phases.total().round(), 90.0);
    }

    #[test]
    fn har_entries_carry_request_response_body_and_timings() {
        let body = Body { text: "eyJvayI6dHJ1ZX0=".to_owned(), base64: true };
        let har = build(&[request()], vec![Some(body)], "1.0.0");
        let log = &har["log"];
        assert_eq!(log["version"], "1.2");
        assert_eq!(log["creator"]["name"], "hba");
        let entry = &log["entries"][0];
        assert_eq!(entry["startedDateTime"], "2023-11-14T22:13:20.500Z");
        assert_eq!(entry["request"]["method"], "POST");
        assert_eq!(entry["request"]["httpVersion"], "HTTP/2");
        assert_eq!(entry["request"]["postData"], json!({"mimeType": "application/json", "text": "{\"n\":1}"}));
        assert_eq!(entry["request"]["bodySize"], 7);
        assert_eq!(
            entry["request"]["queryString"],
            json!([{"name": "page", "value": "2"}, {"name": "q", "value": "a%20b"}])
        );
        assert_eq!(entry["response"]["status"], 302);
        assert_eq!(entry["response"]["redirectURL"], "/next");
        assert_eq!(entry["response"]["headers"].as_array().map(Vec::len), Some(3), "set-cookie lines are split");
        assert_eq!(
            entry["response"]["cookies"],
            json!([
                {"name": "a", "value": "1", "httpOnly": false, "secure": false},
                {"name": "b", "value": "2", "httpOnly": false, "secure": false},
            ])
        );
        assert_eq!(entry["request"]["cookies"], json!([]));
        assert_eq!(
            entry["response"]["content"],
            json!({"size": 11, "mimeType": "application/json", "text": "eyJvayI6dHJ1ZX0=", "encoding": "base64"})
        );
        assert_eq!(entry["response"]["bodySize"], 900);
        assert_eq!(entry["serverIPAddress"], "2001:db8::1");
        assert_eq!(entry["timings"]["wait"], 50.0);
        assert_eq!(entry["time"].as_f64().map(f64::round), Some(89.0));
        assert_eq!(entry["_resourceType"], "fetch");
    }

    #[test]
    fn bodies_measure_their_decoded_bytes() {
        let body = |text: &str, base64: bool| Body { text: text.to_owned(), base64 };
        assert_eq!(body("eyJvayI6dHJ1ZX0=", true).decoded_len(), 11);
        assert_eq!(body("YWI=", true).decoded_len(), 2);
        assert_eq!(body("YQ==", true).decoded_len(), 1);
        assert_eq!(body("héllo", false).decoded_len(), 6);
    }

    #[test]
    fn cookies_are_parsed_from_their_headers() {
        let cookies = request_cookies(&json!({"cookie": "sid=abc; theme = dark;flag;\nlast=x=y"}));
        assert_eq!(
            cookies,
            [
                json!({"name": "sid", "value": "abc"}),
                json!({"name": "theme", "value": "dark"}),
                json!({"name": "", "value": "flag"}),
                json!({"name": "last", "value": "x=y"}),
            ]
        );
        let headers = json!({"Set-Cookie": concat!(
            "sid=abc; Path=/app; Domain=.api.test; Expires=Wed, 21 Oct 2015 07:28:00 GMT; HttpOnly; Secure; SameSite=Lax\n",
            "short=1; max-age=60; expires=Thu, 01-Jan-1970 00:00:00 GMT\n",
            "old=2; expires=Sunday, 06-Nov-94 08:49:37 GMT\n",
            "bad=3; expires=someday\n",
            "\n",
        )});
        assert_eq!(
            response_cookies(&headers, 1_700_000_000.5),
            [
                json!({
                    "name": "sid", "value": "abc", "path": "/app", "domain": ".api.test",
                    "expires": "2015-10-21T07:28:00.000Z", "httpOnly": true, "secure": true,
                }),
                json!({
                    "name": "short", "value": "1", "expires": "2023-11-14T22:14:20.500Z",
                    "httpOnly": false, "secure": false,
                }),
                json!({
                    "name": "old", "value": "2", "expires": "1994-11-06T08:49:37.000Z",
                    "httpOnly": false, "secure": false,
                }),
                json!({"name": "bad", "value": "3", "httpOnly": false, "secure": false}),
            ]
        );
    }

    #[test]
    fn cookie_dates_follow_rfc_6265() {
        assert_eq!(cookie_date("Wed, 21 Oct 2015 07:28:00 GMT"), Some(1_445_412_480.0));
        assert_eq!(cookie_date("Thu Jan  1 00:00:00 1970"), Some(0.0));
        assert_eq!(cookie_date("29-Feb-2024 12:00:00"), Some(1_709_208_000.0));
        assert_eq!(cookie_date("01 Jan 1600 00:00:00"), None, "before 1601");
        assert_eq!(cookie_date("32 Jan 2020 00:00:00"), None);
        assert_eq!(cookie_date("01 Jan 2020"), None, "no time");
    }

    #[test]
    fn server_addresses_drop_the_port() {
        assert_eq!(ip_of("1.2.3.4:443"), "1.2.3.4");
        assert_eq!(ip_of("[::1]:8080"), "::1");
        assert_eq!(ip_of("example"), "example");
    }
}
