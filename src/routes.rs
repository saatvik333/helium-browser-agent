//! Request interception rules (block and mock) applied through the Fetch domain.
//!
//! Interception runs only while rules exist: `Fetch.enable` with one request-stage pattern per
//! rule on every page and frame session of the tab, so other requests never pause. Patterns are
//! globs over the full URL (`*` any run, `?` one character, `\` escapes), as Chrome matches them.

use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine as _;
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cdp::Event;
use crate::output;
use crate::page::Page;

/// The tab's interception rules, oldest first; the newest matching rule wins.
#[derive(Debug, Default)]
pub struct Rules {
    list: Vec<Rule>,
}

/// A URL glob and what happens to matching requests.
#[derive(Debug, Clone)]
pub struct Rule {
    pub pattern: String,
    pub action: Action,
}

/// What happens to a matching request.
#[derive(Debug, Clone)]
pub enum Action {
    /// Fail the request as an extension would (`net::ERR_BLOCKED_BY_CLIENT`).
    Block,
    /// Answer without touching the network.
    Mock(Arc<Mock>),
}

/// A canned response.
#[derive(Debug)]
pub struct Mock {
    pub status: u16,
    /// Response headers, `Content-Type` included.
    pub headers: Vec<(String, String)>,
    /// Body, base64-encoded once for every `Fetch.fulfillRequest`.
    body: String,
    body_len: usize,
}

impl Mock {
    /// A response with `status`, `headers` and `body`.
    pub fn new(status: u16, headers: Vec<(String, String)>, body: &[u8]) -> Self {
        Self { status, headers, body: base64::engine::general_purpose::STANDARD.encode(body), body_len: body.len() }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.action {
            Action::Block => write!(f, "block {}", self.pattern),
            Action::Mock(mock) => {
                write!(f, "mock {} → {} (", self.pattern, mock.status)?;
                if let Some(kind) = mock.header("content-type") {
                    write!(f, "{kind}, ")?;
                }
                write!(f, "{})", output::bytes(mock.body_len as u64))
            }
        }
    }
}

impl Rules {
    /// True when nothing is intercepted.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Number of rules.
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Rules, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &Rule> {
        self.list.iter()
    }

    /// Adds a rule, replacing the one with the same pattern.
    pub fn add(&mut self, rule: Rule) {
        self.list.retain(|existing| existing.pattern != rule.pattern);
        self.list.push(rule);
    }

    /// Removes the rule for `pattern`, or every rule; returns how many went.
    pub fn remove(&mut self, pattern: Option<&str>) -> usize {
        let before = self.list.len();
        match pattern {
            Some(pattern) => self.list.retain(|rule| rule.pattern != pattern),
            None => self.list.clear(),
        }
        before - self.list.len()
    }

    /// The newest rule matching `url`.
    pub fn find(&self, url: &str) -> Option<&Rule> {
        self.list.iter().rev().find(|rule| glob_match(&rule.pattern, url))
    }

    /// `Fetch.enable` parameters pausing exactly the requests the rules match.
    fn fetch_params(&self) -> Value {
        let patterns: Vec<Value> =
            self.list.iter().map(|rule| json!({"urlPattern": rule.pattern, "requestStage": "Request"})).collect();
        json!({ "patterns": patterns })
    }
}

/// One element of a glob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    /// `*`: any run of characters, empty included.
    Star,
    /// `?`: exactly one character.
    One,
    Char(char),
}

fn tokens(pattern: &str) -> Vec<Token> {
    let mut chars = pattern.chars();
    let mut tokens = Vec::with_capacity(pattern.len());
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '*' => Token::Star,
            '?' => Token::One,
            '\\' => Token::Char(chars.next().unwrap_or('\\')),
            c => Token::Char(c),
        });
    }
    tokens
}

/// Whether `text` matches the glob `pattern` as a whole.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let tokens = tokens(pattern);
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    // After a mismatch, the last `*` absorbs one more character: (token after it, text position).
    let mut resume: Option<(usize, usize)> = None;
    while t < text.len() {
        match tokens.get(p) {
            Some(Token::Star) => {
                p += 1;
                resume = Some((p, t));
            }
            Some(Token::One) => {
                p += 1;
                t += 1;
            }
            Some(Token::Char(c)) if *c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => {
                let Some((after_star, absorbed)) = resume else {
                    return false;
                };
                p = after_star;
                t = absorbed + 1;
                resume = Some((after_star, t));
            }
        }
    }
    tokens[p..].iter().all(|token| *token == Token::Star)
}

/// Makes every session of the tab pause exactly the requests the current rules match.
pub async fn apply(page: &Page) -> Result<()> {
    let params = {
        let rules = page.routes.lock().expect("routes lock");
        (!rules.is_empty()).then(|| rules.fetch_params())
    };
    let primary = page.session()?;
    let sessions = page.sessions();
    let params = params.as_ref();
    let calls = sessions.iter().map(|(session, _)| async move {
        let result = match params {
            Some(params) => page.cdp.send("Fetch.enable", params, Some(session)).await,
            None => page.cdp.send("Fetch.disable", json!({}), Some(session)).await,
        };
        (session, result)
    });
    for (session, result) in join_all(calls).await {
        // Frames come and go; only the page itself must take the rules.
        if *session == primary {
            result.context("updating request interception")?;
        }
    }
    Ok(())
}

/// Enables interception on a newly attached page or frame session when rules exist.
pub async fn on_session(page: &Page, session: &str) {
    let params = {
        let rules = page.routes.lock().expect("routes lock");
        if rules.is_empty() {
            return;
        }
        rules.fetch_params()
    };
    page.cdp.send("Fetch.enable", params, Some(session)).await.ok();
}

/// A request held by `Fetch.requestPaused`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Paused {
    request_id: String,
    request: PausedRequest,
}

#[derive(Deserialize)]
struct PausedRequest {
    url: String,
    method: String,
    #[serde(default)]
    headers: Value,
}

/// Answers `Fetch.requestPaused`: block, mock or let the request continue.
pub fn on_event(page: &Page, event: &Event) {
    if event.method != "Fetch.requestPaused" {
        return;
    }
    let Ok(paused) = event.parse::<Paused>() else {
        return;
    };
    let Some(session) = event.session.clone() else {
        return;
    };
    let action = page.routes.lock().expect("routes lock").find(&paused.request.url).map(|rule| rule.action.clone());
    let (method, params) = reply(&paused.request_id, action.as_ref(), &paused.request);
    let cdp = page.cdp.clone();
    tokio::spawn(async move {
        cdp.send(method, params, Some(&session)).await.ok();
    });
}

/// The Fetch command answering a paused request.
fn reply(request_id: &str, action: Option<&Action>, request: &PausedRequest) -> (&'static str, Value) {
    let origin = header(&request.headers, "origin");
    match action {
        None => ("Fetch.continueRequest", json!({"requestId": request_id})),
        Some(Action::Block) => {
            ("Fetch.failRequest", json!({"requestId": request_id, "errorReason": "BlockedByClient"}))
        }
        Some(Action::Mock(mock)) => {
            // Chromium sends the CORS preflight of a cross-origin call through interception too;
            // the mock's own answer would lack what the preflight asks to be allowed.
            if request.method == "OPTIONS"
                && let Some(asked) = header(&request.headers, "access-control-request-method")
            {
                return ("Fetch.fulfillRequest", preflight(request_id, origin, asked, request));
            }
            let mut headers: Vec<Value> =
                mock.headers.iter().map(|(name, value)| json!({"name": name, "value": value})).collect();
            // Cross-origin page code could not read the canned response without these.
            if let Some(origin) = origin
                && mock.header("access-control-allow-origin").is_none()
            {
                headers.push(json!({"name": "Access-Control-Allow-Origin", "value": origin}));
                headers.push(json!({"name": "Access-Control-Allow-Credentials", "value": "true"}));
            }
            let params = json!({
                "requestId": request_id,
                "responseCode": mock.status,
                "responseHeaders": headers,
                "body": mock.body,
            });
            ("Fetch.fulfillRequest", params)
        }
    }
}

/// `Fetch.fulfillRequest` parameters allowing what a CORS preflight asks for, as Playwright does.
fn preflight(request_id: &str, origin: Option<&str>, method: &str, request: &PausedRequest) -> Value {
    let mut headers = vec![
        json!({"name": "Access-Control-Allow-Origin", "value": origin.unwrap_or("*")}),
        json!({"name": "Access-Control-Allow-Methods", "value": method}),
        json!({"name": "Access-Control-Allow-Credentials", "value": "true"}),
    ];
    if let Some(asked) = header(&request.headers, "access-control-request-headers") {
        headers.push(json!({"name": "Access-Control-Allow-Headers", "value": asked}));
    }
    json!({"requestId": request_id, "responseCode": 204, "responseHeaders": headers, "body": ""})
}

/// A header of a protocol headers object, by case-insensitive name.
pub fn header<'a>(headers: &'a Value, name: &str) -> Option<&'a str> {
    headers.as_object()?.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).and_then(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_the_whole_url() {
        let cases = [
            ("*", "https://a.test/x", true),
            ("*.png", "https://a.test/img/logo.png", true),
            ("*.png", "https://a.test/logo.png?v=1", false),
            ("*.png*", "https://a.test/logo.png?v=1", true),
            ("https://a.test/api/*", "https://a.test/api/users/1", true),
            ("https://a.test/api/*", "https://b.test/api/users/1", false),
            ("*/api/*/items", "https://a.test/api/v2/items", true),
            ("*/api/*/items", "https://a.test/api/v2/items/3", false),
            ("https://a.test/?", "https://a.test/x", true),
            ("https://a.test/?", "https://a.test/xy", false),
            ("missing.json", "https://a.test/missing.json", false),
            (r"*\*star", "https://a.test/*star", true),
            (r"*\*star", "https://a.test/xstar", false),
            ("**a**b", "xxaxxbxx", false),
            ("**a**b*", "xxaxxbxx", true),
            ("*é*", "https://a.test/café", true),
        ];
        for (pattern, url, expected) in cases {
            assert_eq!(glob_match(pattern, url), expected, "{pattern} vs {url}");
        }
    }

    #[test]
    fn newest_matching_rule_wins_and_patterns_replace() {
        let mut rules = Rules::default();
        rules.add(Rule { pattern: "*".into(), action: Action::Block });
        rules.add(Rule { pattern: "*.json".into(), action: Action::Mock(Arc::new(Mock::new(200, vec![], b"{}"))) });
        assert!(matches!(rules.find("https://a.test/x.json").map(|r| &r.action), Some(Action::Mock(_))));
        assert!(matches!(rules.find("https://a.test/x.png").map(|r| &r.action), Some(Action::Block)));
        rules.add(Rule { pattern: "*".into(), action: Action::Block });
        assert!(matches!(rules.find("https://a.test/x.json").map(|r| &r.action), Some(Action::Block)));
        assert_eq!(rules.len(), 2);
        assert_eq!(rules.remove(Some("*.json")), 1);
        assert_eq!(rules.remove(None), 1);
        assert!(rules.is_empty());
    }

    fn request(method: &str, headers: Value) -> PausedRequest {
        PausedRequest { url: "https://api.test/x".into(), method: method.into(), headers }
    }

    #[test]
    fn mocks_fulfill_with_base64_body_and_cors_for_cross_origin_callers() {
        let mock = Arc::new(Mock::new(201, vec![("Content-Type".into(), "application/json".into())], b"{\"ok\":true}"));
        let action = Action::Mock(Arc::clone(&mock));
        let (method, params) = reply("r1", Some(&action), &request("POST", json!({"Origin": "https://app.test"})));
        assert_eq!(method, "Fetch.fulfillRequest");
        assert_eq!(params["responseCode"], 201);
        assert_eq!(params["body"], "eyJvayI6dHJ1ZX0=");
        let names: Vec<&str> =
            params["responseHeaders"].as_array().expect("headers").iter().filter_map(|h| h["name"].as_str()).collect();
        assert_eq!(names, ["Content-Type", "Access-Control-Allow-Origin", "Access-Control-Allow-Credentials"]);
        let (_, same_origin) = reply("r1", Some(&action), &request("GET", Value::Null));
        assert_eq!(same_origin["responseHeaders"].as_array().map(Vec::len), Some(1));

        let get = request("GET", Value::Null);
        assert_eq!(reply("r2", Some(&Action::Block), &get).0, "Fetch.failRequest");
        assert_eq!(reply("r3", None, &get), ("Fetch.continueRequest", json!({"requestId": "r3"})));
    }

    #[test]
    fn preflights_of_mocked_urls_allow_what_they_ask_for() {
        let action = Action::Mock(Arc::new(Mock::new(500, vec![], b"down")));
        let asked = json!({"origin": "http://127.0.0.1:8765", "Access-Control-Request-Method": "POST",
                           "Access-Control-Request-Headers": "content-type,x-custom"});
        let (method, params) = reply("r4", Some(&action), &request("OPTIONS", asked));
        assert_eq!(
            (method, params["responseCode"].as_u64(), params["body"].as_str()),
            ("Fetch.fulfillRequest", Some(204), Some(""))
        );
        let headers: Vec<(&str, &str)> = params["responseHeaders"]
            .as_array()
            .expect("headers")
            .iter()
            .filter_map(|h| Some((h["name"].as_str()?, h["value"].as_str()?)))
            .collect();
        assert_eq!(
            headers,
            [
                ("Access-Control-Allow-Origin", "http://127.0.0.1:8765"),
                ("Access-Control-Allow-Methods", "POST"),
                ("Access-Control-Allow-Credentials", "true"),
                ("Access-Control-Allow-Headers", "content-type,x-custom"),
            ]
        );
        // A plain OPTIONS request is not a preflight: the mock answers it.
        assert_eq!(
            reply("r5", Some(&action), &request("OPTIONS", json!({"Origin": "https://a.test"}))).1["responseCode"],
            500
        );
    }

    #[test]
    fn rules_describe_themselves() {
        let mock = Mock::new(200, vec![("Content-Type".into(), "text/plain".into())], b"hello");
        assert_eq!(
            Rule { pattern: "*/a".into(), action: Action::Mock(Arc::new(mock)) }.to_string(),
            "mock */a → 200 (text/plain, 5 B)"
        );
        assert_eq!(Rule { pattern: "*.png".into(), action: Action::Block }.to_string(), "block *.png");
    }
}
