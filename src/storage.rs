//! Cookies, web storage, saved login state and permissions of the page's site.
//!
//! Everything is scoped to the current page: cookies its site receives, storage of its origin,
//! permission overrides of its origin in its own profile. Nothing here touches other sites.
//! Permission overrides apply to the whole profile and shadow its own settings; once no attached
//! tab holds one there, `hba` removes them ([`reset_unheld`]).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::browser::Browser;
use crate::cli::{CookiesArgs, CookiesOp, PermsOp, SaveLoad, StateArgs, StorageArea, StorageArgs, StorageOp};
use crate::daemon::Cx;
use crate::page::{Arg, Page};

/// Characters of a cookie or storage value shown in lists.
const VALUE_CHARS: usize = 60;

/// A cookie as the browser protocol reports it; also the cookie format of `state` files
/// (Playwright's `storageState` layout).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    /// Unix seconds; -1 for session cookies.
    #[serde(default = "session_expiry")]
    expires: f64,
    #[serde(default)]
    http_only: bool,
    #[serde(default)]
    secure: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    same_site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    partition_key: Option<Value>,
}

fn session_expiry() -> f64 {
    -1.0
}

impl Cookie {
    fn is_session(&self) -> bool {
        self.expires <= 0.0
    }

    /// `name=value domain/path expiry flags`, the value cut to [`VALUE_CHARS`].
    fn line(&self) -> String {
        let mut line =
            format!("{}={} {}{}", self.name, crate::output::truncate(&self.value, VALUE_CHARS), self.domain, self.path);
        if self.is_session() {
            line.push_str(" session");
        } else {
            let _ = write!(line, " expires {}", crate::output::utc_minute(self.expires));
        }
        if self.http_only {
            line.push_str(" HttpOnly");
        }
        if self.secure {
            line.push_str(" Secure");
        }
        if let Some(same_site) = &self.same_site {
            let _ = write!(line, " SameSite={same_site}");
        }
        if self.partition_key.is_some() {
            line.push_str(" Partitioned");
        }
        line
    }

    /// The `Network.CookieParam` that recreates this cookie. Host-only cookies are set through
    /// a URL, because an explicit domain would widen them to subdomains.
    fn param(&self) -> Value {
        let mut param = json!({"name": self.name, "value": self.value, "path": self.path,
                               "secure": self.secure, "httpOnly": self.http_only});
        if self.domain.starts_with('.') {
            param["domain"] = json!(self.domain);
        } else {
            let scheme = if self.secure { "https" } else { "http" };
            param["url"] = json!(format!("{scheme}://{}{}", self.domain, self.path));
        }
        if !self.is_session() {
            param["expires"] = json!(self.expires);
        }
        if let Some(same_site) = &self.same_site {
            param["sameSite"] = json!(same_site);
        }
        match &self.partition_key {
            // Playwright writes the top-level site as a plain string.
            Some(Value::String(site)) => {
                param["partitionKey"] = json!({"topLevelSite": site, "hasCrossSiteAncestor": false});
            }
            Some(key) => param["partitionKey"] = key.clone(),
            None => {}
        }
        param
    }

    /// Whether the cookie is sent to `host`: its domain is the host or a parent of it.
    fn reaches(&self, host: &str) -> bool {
        domain_matches(&self.domain, host)
    }
}

/// Cookie domain matching (RFC 6265 §5.1.3), ignoring the leading dot of domain cookies.
fn domain_matches(domain: &str, host: &str) -> bool {
    let domain = domain.strip_prefix('.').unwrap_or(domain).to_ascii_lowercase();
    let host = host.to_ascii_lowercase();
    host == domain || host.strip_suffix(domain.as_str()).is_some_and(|rest| rest.ends_with('.'))
}

/// `scheme://host[:port]` of an http(s) URL, the default port dropped; `None` for other URLs.
pub fn web_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host).to_ascii_lowercase();
    if authority.is_empty() {
        return None;
    }
    let default_port = if scheme == "http" { ":80" } else { ":443" };
    let authority = authority.strip_suffix(default_port).unwrap_or(&authority);
    Some(format!("{scheme}://{authority}"))
}

/// Host of an http(s) URL, without the port.
fn web_host(url: &str) -> Option<String> {
    let origin = web_origin(url)?;
    let authority = origin.split_once("://")?.1;
    let host = match authority.rsplit_once(':') {
        Some((host, port))
            if !host.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && !authority.ends_with(']') =>
        {
            host
        }
        _ => authority,
    };
    Some(host.to_owned())
}

/// The page's browser context (its profile).
pub async fn browser_context(page: &Page) -> Result<String> {
    let info: Value = page.cdp.call("Target.getTargetInfo", json!({"targetId": page.tab}), None).await?;
    info["targetInfo"]["browserContextId"].as_str().map(str::to_owned).context("the tab reports no browser context")
}

/// Overrides one permission for `origin` in `context`, leaving its other permissions alone.
pub async fn set_permission(page: &Page, descriptor: &Value, setting: &str, origin: &str, context: &str) -> Result<()> {
    page.cdp
        .send(
            "Browser.setPermission",
            json!({"permission": descriptor, "setting": setting, "origin": origin, "browserContextId": context}),
            None,
        )
        .await
}

/// Permission overrides `hba` made for a tab and what holds each.
#[derive(Debug, Default)]
pub struct Grants {
    list: Vec<Grant>,
}

#[derive(Debug)]
struct Grant {
    descriptor: Value,
    origin: String,
    context: String,
    perms: bool,
    location: bool,
}

impl Grant {
    fn hold(&mut self, holder: Holder, held: bool) {
        match holder {
            Holder::Perms => self.perms = held,
            Holder::Location => self.location = held,
        }
    }

    fn held(&self) -> bool {
        self.perms || self.location
    }
}

/// What a permission was granted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    /// `hba perms grant`: until `hba perms reset` or the tab's release.
    Perms,
    /// Location emulation: until it ends.
    Location,
}

/// Grants `descriptors` to `origin` for `holder`. Each override is recorded before it is made,
/// so one a cancelled command made is still removed.
pub async fn grant(page: &Page, descriptors: &[Value], origin: &str, holder: Holder) -> Result<()> {
    let context = browser_context(page).await?;
    let fresh: Vec<&Value> = {
        let mut grants = page.permissions.lock().expect("permissions lock");
        let mut fresh = Vec::new();
        for descriptor in descriptors {
            let held = grants
                .list
                .iter_mut()
                .find(|grant| grant.descriptor == *descriptor && grant.origin == origin && grant.context == context);
            if let Some(grant) = held {
                grant.hold(holder, true);
                continue;
            }
            let mut grant = Grant {
                descriptor: descriptor.clone(),
                origin: origin.to_owned(),
                context: context.clone(),
                perms: false,
                location: false,
            };
            grant.hold(holder, true);
            grants.list.push(grant);
            fresh.push(descriptor);
        }
        fresh
    };
    let context = context.as_str();
    let results = join_all(fresh.iter().map(|descriptor| async move {
        set_permission(page, descriptor, "granted", origin, context)
            .await
            .with_context(|| format!("granting {}", descriptor["name"]))
    }))
    .await;
    // Grants that took effect stay recorded even when another failed, so they are removed later.
    let mut failure = None;
    for (descriptor, result) in fresh.into_iter().zip(results) {
        if let Err(error) = result {
            page.permissions
                .lock()
                .expect("permissions lock")
                .list
                .retain(|grant| grant.descriptor != *descriptor || grant.origin != origin || grant.context != context);
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

/// Ends what `holder` was granted on the tab, removing the overrides nothing holds any more.
pub async fn end(browser: &Browser, page: &Page, holder: Holder) -> Result<()> {
    let contexts: Vec<String> = {
        let mut grants = page.permissions.lock().expect("permissions lock");
        for grant in &mut grants.list {
            grant.hold(holder, false);
        }
        let (held, ended): (Vec<Grant>, Vec<Grant>) =
            std::mem::take(&mut grants.list).into_iter().partition(Grant::held);
        grants.list = held;
        ended.into_iter().map(|grant| grant.context).collect()
    };
    reset_unheld(browser, contexts).await
}

/// Forgets the overrides `hba` made for a tab it closed or released (already gone from the
/// browser's attached pages) and removes them once nothing holds one in their profile. Failures
/// are ignored (the browser may be gone).
pub async fn on_release(browser: &Browser, page: &Page) {
    let grants = std::mem::take(&mut page.permissions.lock().expect("permissions lock").list);
    reset_unheld(browser, grants.into_iter().map(|grant| grant.context).collect()).await.ok();
}

/// Removes the permission overrides of each of `contexts` in which no attached tab holds a grant.
/// `Browser.resetPermissions` clears the profile's DevTools overrides only, so its own settings
/// apply again; the protocol has no call that drops a single override.
async fn reset_unheld(browser: &Browser, mut contexts: Vec<String>) -> Result<()> {
    contexts.sort_unstable();
    contexts.dedup();
    if contexts.is_empty() {
        return Ok(());
    }
    let pages = browser.pages();
    contexts.retain(|context| {
        !pages.iter().any(|page| {
            page.permissions.lock().expect("permissions lock").list.iter().any(|grant| grant.context == *context)
        })
    });
    let calls = contexts
        .iter()
        .map(|context| browser.cdp.send("Browser.resetPermissions", json!({"browserContextId": context}), None));
    join_all(calls).await.into_iter().collect::<Result<Vec<()>>>().map(drop)
}

/// Permission descriptors for a name: protocol and Playwright names such as `clipboardReadWrite`
/// or `videoCapture`, or the web's own names such as `clipboard-read` or `camera`.
fn descriptors(name: &str) -> Vec<Value> {
    let named = |names: &[&str]| -> Vec<Value> { names.iter().map(|name| json!({"name": name})).collect() };
    match name {
        "clipboardReadWrite" => named(&["clipboard-read", "clipboard-write"]),
        "clipboardSanitizedWrite" => named(&["clipboard-write"]),
        "audioCapture" => named(&["microphone"]),
        "videoCapture" => named(&["camera"]),
        "durableStorage" => named(&["persistent-storage"]),
        "wakeLockScreen" => named(&["screen-wake-lock"]),
        "wakeLockSystem" => named(&["system-wake-lock"]),
        "sensors" => named(&["accelerometer", "gyroscope", "magnetometer", "ambient-light-sensor"]),
        "midiSysex" => vec![json!({"name": "midi", "sysex": true})],
        _ => {
            // camelCase → kebab-case: `backgroundSync` → `background-sync`.
            let mut kebab = String::with_capacity(name.len() + 4);
            for c in name.chars() {
                if c.is_ascii_uppercase() {
                    kebab.push('-');
                    kebab.push(c.to_ascii_lowercase());
                } else {
                    kebab.push(c);
                }
            }
            vec![json!({"name": kebab})]
        }
    }
}

/// The URL whose cookies a command works on: `--url` or the page's, http(s) only.
fn cookie_url(page: &Page, flag: Option<&str>) -> Result<(String, String)> {
    let url = flag.map_or_else(|| page.url(), str::to_owned);
    let host = web_host(&url).with_context(|| match flag {
        Some(_) => format!("--url must be an http(s) URL, not {url:?}"),
        None => format!(
            "the page ({}) has no cookies; open an http(s) page or pass --url",
            crate::output::short_url(&url, 80)
        ),
    })?;
    Ok((url, host))
}

/// `Network.getCookies` and `Storage.getCookies` results.
#[derive(Deserialize)]
struct Cookies {
    cookies: Vec<Cookie>,
}

/// Cookies the browser sends to `url`.
async fn cookies_for(page: &Page, url: &str) -> Result<Vec<Cookie>> {
    let session = page.session()?;
    let found: Cookies = page.cdp.call("Network.getCookies", json!({"urls": [url]}), Some(&session)).await?;
    Ok(found.cookies)
}

/// Every cookie of the profile that reaches `host`, on any path.
async fn site_cookies(page: &Page, host: &str) -> Result<Vec<Cookie>> {
    let context = browser_context(page).await?;
    let all: Cookies = page.cdp.call("Storage.getCookies", json!({"browserContextId": context}), None).await?;
    Ok(all.cookies.into_iter().filter(|cookie| cookie.reaches(host)).collect())
}

pub async fn cookies(cx: &mut Cx<'_>, args: CookiesArgs) -> Result<()> {
    let page = cx.page().await?;
    let (url, host) = cookie_url(&page, args.url.as_deref())?;
    let session = page.session()?;
    match args.op {
        None => {
            let cookies = cookies_for(&page, &url).await?;
            if cx.json {
                cx.say(serde_json::to_string(&cookies)?);
            } else if cookies.is_empty() {
                cx.say(format!("no cookies for {}", crate::output::short_url(&url, 100)));
            } else {
                let lines: Vec<String> = cookies.iter().map(Cookie::line).collect();
                crate::inspect::emit_budgeted(
                    cx,
                    &lines.join("\n"),
                    8000,
                    "cookies",
                    "txt",
                    "`hba cookies --json` gives full values",
                )?;
            }
        }
        Some(CookiesOp::Set { name, value, domain, path, secure, http_only, same_site, expires }) => {
            let mut params = json!({"name": name, "value": value, "url": url, "path": path.as_deref().unwrap_or("/"),
                                    "secure": secure, "httpOnly": http_only});
            if let Some(domain) = &domain {
                if !domain_matches(domain, &host) {
                    bail!("--domain {domain} does not cover {host}; use {host} or a parent domain of it");
                }
                params["domain"] = json!(domain);
            }
            if let Some(same_site) = &same_site {
                let canonical = match same_site.to_ascii_lowercase().as_str() {
                    "strict" => "Strict",
                    "lax" => "Lax",
                    "none" => "None",
                    _ => bail!("--same-site is Strict, Lax or None"),
                };
                params["sameSite"] = json!(canonical);
            }
            if let Some(expires) = expires {
                params["expires"] = json!(expires);
            }
            page.cdp
                .send("Network.setCookie", params, Some(&session))
                .await
                .with_context(|| format!("setting cookie {name}"))?;
            let stored = site_cookies(&page, &host).await?;
            let path = path.as_deref().unwrap_or("/");
            let Some(cookie) = stored.iter().find(|cookie| cookie.name == name && cookie.path == path) else {
                bail!(
                    "{} did not keep cookie {name} (an expiry in the past, or SameSite=None without --secure)",
                    cx.config()?.browser.name()
                );
            };
            cx.say(format!("set {}", cookie.line()));
        }
        Some(CookiesOp::Rm { name }) => {
            let matching = cookies_for(&page, &url).await?.into_iter().filter(|cookie| cookie.name == name).count();
            if matching == 0 {
                bail!(
                    "no cookie named {name} is sent to {}; `hba cookies` lists them",
                    crate::output::short_url(&url, 100)
                );
            }
            page.cdp.send("Network.deleteCookies", json!({"name": name, "url": url}), Some(&session)).await?;
            cx.say(format!("removed {name} ({})", crate::output::count(matching, "cookie")));
        }
        Some(CookiesOp::Clear) => {
            let cookies = site_cookies(&page, &host).await?;
            let deletions = cookies.iter().map(|cookie| {
                let mut params = json!({"name": cookie.name, "domain": cookie.domain, "path": cookie.path});
                if let Some(key) = &cookie.partition_key {
                    params["partitionKey"] = key.clone();
                }
                page.cdp.send("Network.deleteCookies", params, Some(&session))
            });
            join_all(deletions).await.into_iter().collect::<Result<Vec<()>>>()?;
            cx.say(format!("removed {} reaching {host}", crate::output::count(cookies.len(), "cookie")));
        }
    }
    Ok(())
}

/// Reads or changes the page origin's storage from `hba`'s world. Returns the origin and the
/// operation's result, or the reason storage is unavailable (opaque origins, blocked storage).
const STORAGE: &str = "function(area, op, key, value) { \
    const origin = this.window.location.origin; \
    try { \
      const s = this.window[area]; \
      if (op === 'list') { const items = []; for (let i = 0; i < s.length; i++) { const k = s.key(i); items.push([k, s.getItem(k)]); } return {origin, items}; } \
      if (op === 'get') return {origin, value: s.getItem(key)}; \
      if (op === 'set') { s.setItem(key, value); return {origin}; } \
      if (op === 'rm') { const had = s.getItem(key) !== null; s.removeItem(key); return {origin, had}; } \
      if (op === 'clear') { const count = s.length; s.clear(); return {origin, count}; } \
      if (op === 'replace') { s.clear(); for (const [k, v] of value) s.setItem(k, v); return {origin, count: value.length}; } \
    } catch (e) { return {origin, error: String(e && e.message || e)}; } \
    return {origin, error: 'unknown operation ' + op}; }";

#[derive(Debug, Default, Deserialize)]
struct Outcome {
    origin: String,
    #[serde(default)]
    items: Vec<(String, String)>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    had: bool,
    #[serde(default)]
    count: usize,
    #[serde(default)]
    error: Option<String>,
}

impl StorageArea {
    fn property(self) -> &'static str {
        match self {
            StorageArea::Local => "localStorage",
            StorageArea::Session => "sessionStorage",
        }
    }
}

async fn storage_call(page: &Page, area: StorageArea, op: &str, key: &str, value: Value) -> Result<Outcome> {
    let world = page.main_world().await?;
    let result = page
        .call(
            &world,
            STORAGE,
            &[Arg::Value(json!(area.property())), Arg::Value(json!(op)), Arg::Value(json!(key)), Arg::Value(value)],
        )
        .await?;
    let outcome: Outcome = serde_json::from_value(result).context("decoding the storage result")?;
    if let Some(error) = &outcome.error {
        bail!("{} of {} is unavailable: {error}", area.property(), origin_label(&outcome.origin));
    }
    Ok(outcome)
}

fn origin_label(origin: &str) -> &str {
    if origin.is_empty() || origin == "null" { "this page (opaque origin)" } else { origin }
}

pub async fn storage(cx: &mut Cx<'_>, args: StorageArgs) -> Result<()> {
    let page = cx.page().await?;
    let area = args.area;
    let name = area.property();
    match args.op {
        None => {
            let outcome = storage_call(&page, area, "list", "", Value::Null).await?;
            let origin = origin_label(&outcome.origin);
            if cx.json {
                let map: serde_json::Map<String, Value> =
                    outcome.items.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
                cx.say(serde_json::to_string(&map)?);
            } else if outcome.items.is_empty() {
                cx.say(format!("{name} of {origin} is empty"));
            } else {
                let mut text = format!("{name} of {origin} ({})", crate::output::count(outcome.items.len(), "key"));
                for (key, value) in &outcome.items {
                    let _ = write!(text, "\n{key}: {}", crate::output::truncate(value, VALUE_CHARS));
                }
                crate::inspect::emit_budgeted(
                    cx,
                    &text,
                    8000,
                    "storage",
                    "txt",
                    "`hba storage get KEY` prints one value in full",
                )?;
            }
        }
        Some(StorageOp::Get { key }) => {
            let outcome = storage_call(&page, area, "get", &key, Value::Null).await?;
            let Some(value) = outcome.value else {
                bail!(
                    "{name} of {} has no key {key:?}; `hba storage {}` lists them",
                    origin_label(&outcome.origin),
                    area_arg(area)
                );
            };
            cx.say(if cx.json { serde_json::to_string(&value)? } else { value });
        }
        Some(StorageOp::Set { key, value }) => {
            let outcome = storage_call(&page, area, "set", &key, json!(value)).await?;
            cx.say(format!("set {key} in {name} of {}", origin_label(&outcome.origin)));
        }
        Some(StorageOp::Rm { key }) => {
            let outcome = storage_call(&page, area, "rm", &key, Value::Null).await?;
            if !outcome.had {
                bail!("{name} of {} has no key {key:?}", origin_label(&outcome.origin));
            }
            cx.say(format!("removed {key} from {name} of {}", origin_label(&outcome.origin)));
        }
        Some(StorageOp::Clear) => {
            let outcome = storage_call(&page, area, "clear", "", Value::Null).await?;
            cx.say(format!(
                "cleared {} from {name} of {}",
                crate::output::count(outcome.count, "key"),
                origin_label(&outcome.origin)
            ));
        }
    }
    Ok(())
}

fn area_arg(area: StorageArea) -> &'static str {
    match area {
        StorageArea::Local => "local",
        StorageArea::Session => "session",
    }
}

/// A `state` file: Playwright's `storageState` layout plus session storage.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedState {
    cookies: Vec<Cookie>,
    #[serde(default)]
    origins: Vec<SavedOrigin>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedOrigin {
    origin: String,
    #[serde(default)]
    local_storage: Vec<Item>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    session_storage: Vec<Item>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Item {
    name: String,
    value: String,
}

fn items(pairs: Vec<(String, String)>) -> Vec<Item> {
    pairs.into_iter().map(|(name, value)| Item { name, value }).collect()
}

pub async fn state(cx: &mut Cx<'_>, args: StateArgs) -> Result<()> {
    let page = cx.page().await?;
    let path = cx.path(&args.path);
    let url = page.url();
    let host = web_host(&url).with_context(|| {
        format!("state needs an http(s) page; the tab shows {}", crate::output::short_url(&url, 80))
    })?;
    match args.action {
        SaveLoad::Save => save_state(cx, &page, &host, &path).await,
        SaveLoad::Load => load_state(cx, &page, &host, &path).await,
    }
}

async fn save_state(cx: &mut Cx<'_>, page: &Page, host: &str, path: &Path) -> Result<()> {
    let (cookies, local, session) = tokio::join!(
        site_cookies(page, host),
        storage_call(page, StorageArea::Local, "list", "", Value::Null),
        storage_call(page, StorageArea::Session, "list", "", Value::Null),
    );
    let (cookies, local, session) = (cookies?, local?, session?);
    let counts = format!(
        "{}, {} and {}",
        crate::output::count(cookies.len(), "cookie"),
        crate::output::count(local.items.len(), "localStorage key"),
        crate::output::count(session.items.len(), "sessionStorage key"),
    );
    let state = SavedState {
        cookies,
        origins: vec![SavedOrigin {
            origin: local.origin.clone(),
            local_storage: items(local.items),
            session_storage: items(session.items),
        }],
    };
    crate::output::write_private(path, serde_json::to_string_pretty(&state)?.as_bytes())?;
    cx.say(format!("saved {} ({counts} of {})", path.display(), local.origin));
    cx.say("it holds login secrets, so only you can read it (mode 600)");
    Ok(())
}

/// Splits saved cookies into those `host` receives and the domains of the others, each once.
fn reaching(cookies: Vec<Cookie>, host: &str) -> (Vec<Cookie>, Vec<String>) {
    let (kept, skipped): (Vec<Cookie>, Vec<Cookie>) = cookies.into_iter().partition(|cookie| cookie.reaches(host));
    let mut domains: Vec<String> =
        skipped.into_iter().map(|cookie| cookie.domain.trim_start_matches('.').to_ascii_lowercase()).collect();
    domains.sort_unstable();
    domains.dedup();
    (kept, domains)
}

async fn load_state(cx: &mut Cx<'_>, page: &Page, host: &str, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let state: SavedState =
        serde_json::from_str(&text).with_context(|| format!("{} is not a state file", path.display()))?;
    let (cookies, other_sites) = reaching(state.cookies, host);
    let session = page.session()?;
    if !cookies.is_empty() {
        let params: Vec<Value> = cookies.iter().map(Cookie::param).collect();
        page.cdp
            .send("Network.setCookies", json!({"cookies": params}), Some(&session))
            .await
            .context("restoring cookies")?;
    }
    let origin = web_origin(&page.url()).unwrap_or_default();
    let mut restored = vec![crate::output::count(cookies.len(), "cookie")];
    let mut others = Vec::new();
    for saved in state.origins {
        if web_origin(&saved.origin).as_deref() != Some(origin.as_str()) {
            others.push(saved.origin);
            continue;
        }
        let areas = [(StorageArea::Local, saved.local_storage), (StorageArea::Session, saved.session_storage)];
        for (area, entries) in areas {
            if entries.is_empty() {
                continue;
            }
            let pairs: Vec<(String, String)> = entries.into_iter().map(|item| (item.name, item.value)).collect();
            let outcome = storage_call(page, area, "replace", "", json!(pairs)).await?;
            restored.push(format!("{} ({})", area.property(), crate::output::count(outcome.count, "key")));
        }
    }
    cx.say(format!("restored {} for {origin}", restored.join(", ")));
    if !other_sites.is_empty() {
        cx.say(format!("cookies of {} are for other sites: open each and load again", other_sites.join(", ")));
    }
    if !others.is_empty() {
        cx.say(format!("storage of {} is for other origins: open each and load again", others.join(", ")));
    }
    cx.say("reload so the page uses it: `hba reload`");
    Ok(())
}

pub async fn perms(cx: &mut Cx<'_>, op: PermsOp) -> Result<()> {
    let page = cx.page().await?;
    match op {
        PermsOp::Grant { permissions, origin } => {
            let origin = match origin {
                Some(origin) => web_origin(&origin)
                    .with_context(|| format!("--origin must be an http(s) origin, not {origin:?}"))?,
                None => web_origin(&page.url()).context("the page has no http(s) origin; pass --origin")?,
            };
            let descriptors: Vec<Value> = permissions.iter().flat_map(|name| descriptors(name)).collect();
            grant(&page, &descriptors, &origin, Holder::Perms).await?;
            cx.say(format!(
                "granted {} to {origin} in this profile until hba lets go of {} and of any other tab with a grant there",
                permissions.join(", "),
                cx.tab_number(&page.tab)
            ));
        }
        PermsOp::Reset => {
            let context = browser_context(&page).await?;
            page.cdp.send("Browser.resetPermissions", json!({"browserContextId": context}), None).await?;
            // The reset removed the overrides every tab of the profile held.
            for page in cx.browser().await?.pages() {
                page.permissions.lock().expect("permissions lock").list.retain(|grant| grant.context != context);
            }
            cx.say("removed every permission override hba made in this profile");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(domain: &str) -> Cookie {
        Cookie {
            name: "sid".into(),
            value: "abc".into(),
            domain: domain.into(),
            path: "/".into(),
            expires: -1.0,
            http_only: true,
            secure: false,
            same_site: Some("Lax".into()),
            partition_key: None,
        }
    }

    #[test]
    fn cookie_lines_are_compact() {
        let session = cookie("example.com");
        assert_eq!(session.line(), "sid=abc example.com/ session HttpOnly SameSite=Lax");
        let long = Cookie {
            value: "x".repeat(100),
            expires: 1_798_761_600.0,
            secure: true,
            same_site: None,
            ..cookie(".example.com")
        };
        let line = long.line();
        assert!(line.contains(&format!("{}…", "x".repeat(VALUE_CHARS - 1))), "{line}");
        assert!(line.ends_with(".example.com/ expires 2027-01-01T00:00Z HttpOnly Secure"), "{line}");
    }

    #[test]
    fn loading_state_keeps_only_the_sites_cookies() {
        let saved =
            vec![cookie(".example.com"), cookie("other.test"), cookie("www.example.com"), cookie(".Other.test")];
        let (kept, skipped) = reaching(saved, "www.example.com");
        assert_eq!(
            kept.iter().map(|cookie| cookie.domain.as_str()).collect::<Vec<_>>(),
            [".example.com", "www.example.com"]
        );
        assert_eq!(skipped, ["other.test"], "each other site once");
        let (kept, skipped) = reaching(vec![cookie("example.com")], "example.com");
        assert_eq!((kept.len(), skipped.len()), (1, 0));
    }

    #[test]
    fn cookies_reach_their_host_and_subdomains_only() {
        assert!(cookie(".example.com").reaches("www.example.com"));
        assert!(cookie("example.com").reaches("example.com"));
        assert!(!cookie("example.com").reaches("badexample.com"));
        assert!(!cookie("www.example.com").reaches("example.com"));
        assert!(cookie("127.0.0.1").reaches("127.0.0.1"));
    }

    #[test]
    fn restored_cookies_keep_their_scope() {
        let host_only = cookie("example.com").param();
        assert_eq!(host_only["url"], "http://example.com/");
        assert!(host_only.get("domain").is_none() && host_only.get("expires").is_none());
        let domain = Cookie { expires: 2e9, ..cookie(".example.com") }.param();
        assert_eq!((domain["domain"].as_str(), domain["expires"].as_f64()), (Some(".example.com"), Some(2e9)));
        let partitioned = Cookie { partition_key: Some(json!("https://top.example")), ..cookie("a.com") }.param();
        assert_eq!(partitioned["partitionKey"]["topLevelSite"], "https://top.example");
    }

    #[test]
    fn state_files_read_playwright_storage_state() {
        let text = r#"{"cookies":[{"name":"a","value":"1","domain":".x.com","path":"/","expires":-1,"httpOnly":false,"secure":true,"sameSite":"None"}],
                       "origins":[{"origin":"https://x.com","localStorage":[{"name":"k","value":"v"}]}]}"#;
        let state: SavedState = serde_json::from_str(text).expect("valid");
        assert_eq!(state.cookies[0].same_site.as_deref(), Some("None"));
        assert_eq!(state.origins[0].local_storage[0].value, "v");
        assert!(state.origins[0].session_storage.is_empty());
    }

    #[test]
    fn origins_and_hosts() {
        assert_eq!(web_origin("http://127.0.0.1:8765/a?b").as_deref(), Some("http://127.0.0.1:8765"));
        assert_eq!(web_origin("HTTPS://user@Example.com:443/").as_deref(), Some("https://example.com"));
        assert_eq!(web_origin("file:///tmp/x.html"), None);
        assert_eq!(web_origin("about:blank"), None);
        assert_eq!(web_host("http://127.0.0.1:8765/").as_deref(), Some("127.0.0.1"));
        assert_eq!(web_host("http://[::1]:8080/").as_deref(), Some("[::1]"));
        assert_eq!(web_host("https://[::1]/").as_deref(), Some("[::1]"));
    }

    #[test]
    fn permission_names_map_to_descriptors() {
        assert_eq!(descriptors("geolocation"), vec![json!({"name": "geolocation"})]);
        assert_eq!(descriptors("clipboardReadWrite").len(), 2);
        assert_eq!(descriptors("videoCapture"), vec![json!({"name": "camera"})]);
        assert_eq!(descriptors("backgroundSync"), vec![json!({"name": "background-sync"})]);
        assert_eq!(descriptors("clipboard-read"), vec![json!({"name": "clipboard-read"})]);
        assert_eq!(descriptors("midiSysex"), vec![json!({"name": "midi", "sysex": true})]);
    }
}
