//! Device, user agent, locale, timezone, location, media and network emulation for one tab.
//!
//! Overrides live in the tab's protocol sessions: they never touch Helium's settings and vanish
//! when `hba` detaches. Every page and out-of-process frame session gets them, including
//! sessions that appear later ([`on_session`]). Location emulation also allows geolocation for the
//! page's origin in the profile, an override [`crate::storage`] removes when nothing holds it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::browser::Browser;
use crate::cli::{ColorScheme, EmulateArgs, MediaType, NetworkPreset};
use crate::daemon::Cx;
use crate::page::Page;
use crate::storage::Holder;

/// Playwright's device descriptors, vendored with its injected script.
const DEVICES: &str = include_str!("../assets/playwright/devices.json");

/// Touch points reported while touch is emulated (Playwright's value).
const TOUCH_POINTS: u32 = 5;

/// Emulation applied to a tab, so it reaches new sessions and can be reported and reset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    device: Option<Chosen>,
    metrics: Option<Metrics>,
    user_agent: Option<String>,
    locale: Option<String>,
    timezone: Option<String>,
    geolocation: Option<Geolocation>,
    scheme: Option<ColorScheme>,
    media: Option<MediaType>,
    reduced_motion: bool,
    network: Option<NetworkPreset>,
    cpu: Option<f64>,
}

/// The device preset in effect and the user agent it brought.
#[derive(Debug, Clone, PartialEq)]
struct Chosen {
    name: String,
    user_agent: String,
}

/// Screen metrics; zero width, height or scale leave that value as the window has it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Metrics {
    width: u32,
    height: u32,
    screen: Option<Size>,
    scale: f64,
    mobile: bool,
    touch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Geolocation {
    latitude: f64,
    longitude: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
struct Size {
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Device {
    user_agent: String,
    viewport: Size,
    #[serde(default)]
    screen: Option<Size>,
    #[serde(rename = "deviceScaleFactor")]
    scale_factor: f64,
    is_mobile: bool,
    has_touch: bool,
}

/// Overrides that are applied (and cleared) together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Metrics,
    Agent,
    Locale,
    Timezone,
    Geolocation,
    Media,
    Network,
    Cpu,
}

const GROUPS: [Group; 8] = [
    Group::Metrics,
    Group::Agent,
    Group::Locale,
    Group::Timezone,
    Group::Geolocation,
    Group::Media,
    Group::Network,
    Group::Cpu,
];

/// Throughput and latency of a network preset; throughput in bytes per second.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Conditions {
    offline: bool,
    latency_ms: f64,
    download: f64,
    upload: f64,
}

impl State {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether device metrics (viewport size, pixel ratio, mobile) are emulated.
    pub fn emulates_metrics(&self) -> bool {
        self.metrics.is_some()
    }

    /// Whether `group` differs between two states.
    fn differs(&self, other: &Self, group: Group) -> bool {
        match group {
            Group::Metrics => self.metrics != other.metrics,
            Group::Agent => {
                self.user_agent != other.user_agent || self.locale != other.locale || self.mobile() != other.mobile()
            }
            Group::Locale => self.locale != other.locale,
            Group::Timezone => self.timezone != other.timezone,
            Group::Geolocation => {
                self.geolocation.as_ref().map(|g| (g.latitude, g.longitude))
                    != other.geolocation.as_ref().map(|g| (g.latitude, g.longitude))
            }
            Group::Media => {
                (self.scheme, self.media, self.reduced_motion) != (other.scheme, other.media, other.reduced_motion)
            }
            Group::Network => self.network != other.network,
            Group::Cpu => self.cpu != other.cpu,
        }
    }

    /// Copies one group's settings from `source` (to undo a group the browser refused).
    fn take_group(&mut self, source: &Self, group: Group) {
        match group {
            Group::Metrics => {
                self.metrics = source.metrics;
                self.device.clone_from(&source.device);
            }
            Group::Agent => self.user_agent.clone_from(&source.user_agent),
            Group::Locale => self.locale.clone_from(&source.locale),
            Group::Timezone => self.timezone.clone_from(&source.timezone),
            Group::Geolocation => self.geolocation.clone_from(&source.geolocation),
            Group::Media => {
                self.scheme = source.scheme;
                self.media = source.media;
                self.reduced_motion = source.reduced_motion;
            }
            Group::Network => self.network = source.network,
            Group::Cpu => self.cpu = source.cpu,
        }
    }

    fn mobile(&self) -> bool {
        self.metrics.is_some_and(|m| m.mobile)
    }

    /// This state with the command's settings applied on top.
    fn merged(&self, args: &EmulateArgs, device: Option<(&str, &Device)>) -> Result<Self> {
        let mut next = self.clone();
        if let Some((name, device)) = device {
            next.device = Some(Chosen { name: name.to_owned(), user_agent: device.user_agent.clone() });
            next.user_agent = Some(device.user_agent.clone());
            next.metrics = Some(Metrics {
                width: device.viewport.width,
                height: device.viewport.height,
                screen: device.screen,
                scale: device.scale_factor,
                mobile: device.is_mobile,
                touch: device.has_touch,
            });
        }
        if args.viewport.is_some() || args.dpr.is_some() || args.mobile {
            let mut metrics = next.metrics.unwrap_or_default();
            if let Some((width, height)) = args.viewport {
                metrics.width = width;
                metrics.height = height;
                metrics.screen = None;
            }
            if let Some(scale) = args.dpr {
                if !(scale > 0.0 && scale <= 10.0) {
                    bail!("--dpr must be between 0 and 10, e.g. 2");
                }
                metrics.scale = scale;
            }
            if args.mobile {
                metrics.mobile = true;
                metrics.touch = true;
            }
            next.metrics = Some(metrics);
        }
        if let Some(user_agent) = &args.ua {
            next.user_agent = (!user_agent.is_empty()).then(|| user_agent.clone());
        }
        if let Some(locale) = &args.locale {
            next.locale = (!locale.is_empty()).then(|| locale.clone());
        }
        if let Some(timezone) = &args.tz {
            next.timezone = (!timezone.is_empty()).then(|| timezone.clone());
        }
        if let Some((latitude, longitude)) = args.geo {
            if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
                bail!("--geo wants LATITUDE,LONGITUDE within ±90,±180, e.g. 12.97,77.59");
            }
            next.geolocation = Some(Geolocation { latitude, longitude });
        }
        if let Some(scheme) = args.scheme {
            next.scheme = Some(scheme);
        }
        if let Some(media) = args.media {
            next.media = Some(media);
        }
        next.reduced_motion |= args.reduced_motion;
        if let Some(network) = args.network {
            next.network = (network != NetworkPreset::None).then_some(network);
        }
        if let Some(cpu) = args.cpu {
            if !(1.0..=100.0).contains(&cpu) {
                bail!("--cpu is a slowdown factor from 1 (none) to 100, e.g. 4");
            }
            next.cpu = (cpu > 1.0).then_some(cpu);
        }
        Ok(next)
    }

    /// The protocol calls that make a session match this state for `group`.
    fn commands(&self, group: Group, is_page: bool) -> Vec<(&'static str, Value)> {
        match group {
            Group::Metrics if !is_page => Vec::new(),
            Group::Metrics => match self.metrics {
                Some(metrics) => {
                    let mut params = json!({"width": metrics.width, "height": metrics.height,
                        "deviceScaleFactor": metrics.scale, "mobile": metrics.mobile});
                    if let Some(screen) = metrics.screen {
                        params["screenWidth"] = json!(screen.width);
                        params["screenHeight"] = json!(screen.height);
                    }
                    if metrics.width > 0 && metrics.height > 0 {
                        params["screenOrientation"] = if metrics.mobile && metrics.width > metrics.height {
                            json!({"angle": 90, "type": "landscapePrimary"})
                        } else if metrics.mobile {
                            json!({"angle": 0, "type": "portraitPrimary"})
                        } else {
                            json!({"angle": 0, "type": "landscapePrimary"})
                        };
                    }
                    let touch = if metrics.touch {
                        json!({"enabled": true, "maxTouchPoints": TOUCH_POINTS})
                    } else {
                        json!({"enabled": false})
                    };
                    vec![("Emulation.setDeviceMetricsOverride", params), ("Emulation.setTouchEmulationEnabled", touch)]
                }
                None => vec![
                    ("Emulation.clearDeviceMetricsOverride", json!({})),
                    ("Emulation.setTouchEmulationEnabled", json!({"enabled": false})),
                ],
            },
            Group::Agent => {
                // An empty user agent keeps Helium's own (and its client hints).
                let mut params = json!({"userAgent": self.user_agent.as_deref().unwrap_or_default()});
                if let Some(locale) = &self.locale {
                    params["acceptLanguage"] = json!(locale);
                }
                if let Some(user_agent) = &self.user_agent {
                    params["userAgentMetadata"] = client_hints(user_agent, self.mobile());
                }
                vec![("Emulation.setUserAgentOverride", params)]
            }
            Group::Locale => {
                vec![(
                    "Emulation.setLocaleOverride",
                    self.locale.as_ref().map_or_else(|| json!({}), |l| json!({"locale": l})),
                )]
            }
            Group::Timezone => {
                vec![(
                    "Emulation.setTimezoneOverride",
                    json!({"timezoneId": self.timezone.as_deref().unwrap_or_default()}),
                )]
            }
            Group::Geolocation => match &self.geolocation {
                Some(geo) => vec![(
                    "Emulation.setGeolocationOverride",
                    json!({"latitude": geo.latitude, "longitude": geo.longitude, "accuracy": 10}),
                )],
                None => vec![("Emulation.clearGeolocationOverride", json!({}))],
            },
            Group::Media => {
                let scheme = match self.scheme {
                    Some(ColorScheme::Light) => "light",
                    Some(ColorScheme::Dark) => "dark",
                    None => "",
                };
                let media = match self.media {
                    Some(MediaType::Screen) => "screen",
                    Some(MediaType::Print) => "print",
                    None => "",
                };
                let motion = if self.reduced_motion { "reduce" } else { "" };
                vec![(
                    "Emulation.setEmulatedMedia",
                    json!({"media": media, "features": [
                        {"name": "prefers-color-scheme", "value": scheme},
                        {"name": "prefers-reduced-motion", "value": motion},
                    ]}),
                )]
            }
            Group::Network => {
                let conditions = self.network.and_then(conditions).unwrap_or(Conditions {
                    offline: false,
                    latency_ms: 0.0,
                    download: -1.0,
                    upload: -1.0,
                });
                vec![(
                    "Network.emulateNetworkConditions",
                    json!({"offline": conditions.offline, "latency": conditions.latency_ms,
                           "downloadThroughput": conditions.download, "uploadThroughput": conditions.upload}),
                )]
            }
            Group::Cpu => vec![("Emulation.setCPUThrottlingRate", json!({"rate": self.cpu.unwrap_or(1.0)}))],
        }
    }

    /// One line naming everything that is emulated.
    fn summary(&self) -> String {
        if self.is_empty() {
            return "no emulation".to_owned();
        }
        let mut parts = Vec::new();
        if let Some(device) = &self.device {
            parts.push(device.name.clone());
        }
        if let Some(metrics) = self.metrics {
            let mut text = String::new();
            if metrics.width > 0 && metrics.height > 0 {
                text = format!("{}x{}", metrics.width, metrics.height);
            }
            if metrics.scale > 0.0 {
                let _ = write!(text, " @{}x", metrics.scale);
            }
            if metrics.mobile {
                text.push_str(" mobile");
            }
            if metrics.touch {
                text.push_str(" touch");
            }
            parts.push(text.trim().to_owned());
        }
        if let Some(user_agent) = &self.user_agent
            && self.device.as_ref().is_none_or(|device| device.user_agent != *user_agent)
        {
            parts.push(format!("ua {:?}", crate::output::truncate(user_agent, 60)));
        }
        if let Some(locale) = &self.locale {
            parts.push(format!("locale {locale}"));
        }
        if let Some(timezone) = &self.timezone {
            parts.push(format!("tz {timezone}"));
        }
        if let Some(geo) = &self.geolocation {
            parts.push(format!("geo {},{}", geo.latitude, geo.longitude));
        }
        match self.scheme {
            Some(ColorScheme::Light) => parts.push("light scheme".to_owned()),
            Some(ColorScheme::Dark) => parts.push("dark scheme".to_owned()),
            None => {}
        }
        match self.media {
            Some(MediaType::Screen) => parts.push("screen media".to_owned()),
            Some(MediaType::Print) => parts.push("print media".to_owned()),
            None => {}
        }
        if self.reduced_motion {
            parts.push("reduced motion".to_owned());
        }
        if let Some(network) = self.network {
            parts.push(format!("network {}", preset_name(network)));
        }
        if let Some(cpu) = self.cpu {
            parts.push(format!("cpu {cpu}x slower"));
        }
        format!("emulating {}", parts.join(", "))
    }
}

/// Throttling presets of the browser's developer tools (`NetworkManager.ts`). They renamed
/// "Fast 3G" to "Slow 4G" in 2024 without changing its numbers, so both give the same conditions.
fn conditions(preset: NetworkPreset) -> Option<Conditions> {
    let throttled = |kbps_down: f64, kbps_up: f64, factor: f64, latency_ms: f64| Conditions {
        offline: false,
        latency_ms,
        download: kbps_down * 1000.0 / 8.0 * factor,
        upload: kbps_up * 1000.0 / 8.0 * factor,
    };
    match preset {
        NetworkPreset::Offline => Some(Conditions { offline: true, latency_ms: 0.0, download: 0.0, upload: 0.0 }),
        NetworkPreset::Slow3g => Some(throttled(500.0, 500.0, 0.8, 400.0 * 5.0)),
        NetworkPreset::Fast3g | NetworkPreset::Slow4g => Some(throttled(1600.0, 750.0, 0.9, 150.0 * 3.75)),
        NetworkPreset::None => None,
    }
}

fn preset_name(preset: NetworkPreset) -> &'static str {
    match preset {
        NetworkPreset::Offline => "offline",
        NetworkPreset::Slow3g => "slow-3g",
        NetworkPreset::Fast3g => "fast-3g",
        NetworkPreset::Slow4g => "slow-4g",
        NetworkPreset::None => "none",
    }
}

/// User-agent client hints matching a user agent string, as Playwright derives them, so
/// `navigator.userAgentData` does not contradict the emulated user agent.
fn client_hints(user_agent: &str, mobile: bool) -> Value {
    let version = |prefix: &str| -> Option<String> {
        let start = user_agent.find(prefix)? + prefix.len();
        let rest = &user_agent[start..];
        let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '_')).unwrap_or(rest.len());
        let found = rest[..end].trim_end_matches(['.', '_']);
        (!found.is_empty()).then(|| found.to_owned())
    };
    let (mut platform, mut platform_version, mut architecture) = ("Windows", String::new(), "x86");
    if let Some(found) = version("Android ") {
        (platform, platform_version, architecture) = ("Android", found, "arm");
    } else if let Some(found) = version("iPhone OS ").or_else(|| version("iPad; CPU OS ")) {
        (platform, platform_version, architecture) = ("iOS", found, "arm");
    } else if let Some(found) = version("Mac OS X ") {
        platform = "macOS";
        platform_version = found;
        if !user_agent.contains("Intel") {
            architecture = "arm";
        }
    } else if let Some(found) = user_agent.find("Windows").and_then(|at| {
        let rest = &user_agent[at..];
        let digits = rest.find(|c: char| c.is_ascii_digit())?;
        let tail = &rest[digits..];
        let end = tail.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(tail.len());
        Some(tail[..end].to_owned())
    }) {
        platform_version = found;
    } else if user_agent.to_ascii_lowercase().contains("linux") {
        platform = "Linux";
    }
    if user_agent.contains("ARM") {
        architecture = "arm";
    }
    json!({"platform": platform, "platformVersion": platform_version, "architecture": architecture,
           "model": "", "mobile": mobile})
}

fn devices() -> Result<BTreeMap<String, Device>> {
    serde_json::from_str(DEVICES).context("decoding the device list")
}

/// Lowercase letters, digits and `+`, so `pixel7` finds "Pixel 7" but "Galaxy S9+" stays distinct.
fn device_key(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '+').map(|c| c.to_ascii_lowercase()).collect()
}

/// The device whose name matches `query` ignoring case, spaces and punctuation.
fn find_device<'a>(devices: &'a BTreeMap<String, Device>, query: &str) -> Result<(&'a str, &'a Device)> {
    let exact = devices.iter().find(|(name, _)| name.eq_ignore_ascii_case(query.trim()));
    let key = device_key(query);
    if let Some((name, device)) = exact.or_else(|| devices.iter().find(|(name, _)| device_key(name) == key)) {
        return Ok((name, device));
    }
    let landscape = key.contains("landscape");
    let similar: Vec<&str> = devices
        .keys()
        .filter(|name| {
            !key.is_empty() && device_key(name).contains(&key) && (landscape || !name.ends_with(" landscape"))
        })
        .map(String::as_str)
        .collect();
    match similar.as_slice() {
        [] => bail!("no device is called {query:?}; `hba emulate --list` shows them all"),
        few if few.len() <= 8 => bail!("no device is called {query:?}; did you mean {}?", few.join(", ")),
        many => bail!(
            "no device is called {query:?}; did you mean {} (or {} more in `hba emulate --list`)?",
            many[..8].join(", "),
            many.len() - 8
        ),
    }
}

/// `hba emulate --list`: device names, landscape variants folded into a count.
fn device_list() -> Result<String> {
    let devices = devices()?;
    let names: Vec<&str> = devices.keys().map(String::as_str).filter(|name| !name.ends_with(" landscape")).collect();
    let landscape = devices.len() - names.len();
    Ok(format!("{}\n({} devices; {landscape} also have a \"NAME landscape\" variant)", names.join(", "), names.len()))
}

pub async fn emulate(cx: &mut Cx<'_>, args: EmulateArgs) -> Result<()> {
    if args.list {
        cx.say(device_list()?);
        return Ok(());
    }
    let page = cx.page().await?;
    let browser = cx.browser().await?;
    let mut current = page.emulation.lock().expect("emulation lock").clone();
    if args.reset {
        current = transition(&browser, &page, &current, State::default()).await?;
    }
    let devices = if args.device.is_some() { devices()? } else { BTreeMap::new() };
    let device = args.device.as_deref().map(|query| find_device(&devices, query)).transpose()?;
    let next = current.merged(&args, device)?;
    let applied = transition(&browser, &page, &current, next).await?;
    cx.say(applied.summary());
    if current.differs(&applied, Group::Agent) && crate::storage::web_origin(&page.url()).is_some() {
        cx.say("the server sees the new user agent or language after `hba reload`");
    }
    Ok(())
}

/// Moves the tab from `current` to `next` and records what took effect. Groups the page
/// session refuses keep their previous settings, and the first refusal is returned.
async fn transition(browser: &Browser, page: &Page, current: &State, next: State) -> Result<State> {
    let starts_location = current.geolocation.is_none() && next.geolocation.is_some();
    let origin = if starts_location {
        Some(crate::storage::web_origin(&page.url()).context(
            "location emulation allows geolocation for the page's origin, so it needs an http(s) page; `hba open URL` first",
        )?)
    } else {
        None
    };
    let (kept, refusal) = match apply(page, current, &next).await {
        Ok(()) => (next, None),
        Err(Refused { groups, error }) => {
            let mut kept = next;
            for group in groups {
                kept.take_group(current, group);
            }
            (kept, Some(error))
        }
    };
    let permission = permit_location(browser, page, current, &kept, origin).await;
    *page.emulation.lock().expect("emulation lock") = kept.clone();
    match (refusal, permission) {
        (Some(error), _) | (None, Err(error)) => Err(error),
        (None, Ok(())) => Ok(kept),
    }
}

/// Groups the page session refused, and the first refusal.
struct Refused {
    groups: Vec<Group>,
    error: anyhow::Error,
}

/// Moves every session of the tab from `from` to `to`, sending only the groups that changed.
/// Failures on frame sessions are ignored (frames come and go); the page session's are returned.
async fn apply(page: &Page, from: &State, to: &State) -> std::result::Result<(), Refused> {
    let primary = page.session().map_err(|error| Refused { groups: GROUPS.to_vec(), error })?;
    let changed: Vec<Group> = GROUPS.into_iter().filter(|group| from.differs(to, *group)).collect();
    let mut calls = Vec::new();
    for (session, is_page) in page.sessions() {
        for group in &changed {
            for (method, params) in to.commands(*group, is_page) {
                let session = Arc::clone(&session);
                calls.push(async move {
                    let result = page.cdp.send(method, params, Some(&session)).await;
                    (*group, session, result)
                });
            }
        }
    }
    let mut refused = Vec::new();
    let mut first = None;
    for (group, session, result) in join_all(calls).await {
        if let Err(error) = result
            && session == primary
        {
            if !refused.contains(&group) {
                refused.push(group);
            }
            first.get_or_insert(error);
        }
    }
    match first {
        None => Ok(()),
        Some(error) => Err(Refused { groups: refused, error }),
    }
}

/// Allows geolocation for `origin` when location emulation starts, and gives the grant up when it
/// ends (or `hba` releases the tab).
async fn permit_location(
    browser: &Browser,
    page: &Page,
    current: &State,
    next: &State,
    origin: Option<String>,
) -> Result<()> {
    if current.geolocation.is_some() && next.geolocation.is_none() {
        return crate::storage::end(browser, page, Holder::Location).await;
    }
    match origin {
        Some(origin) if next.geolocation.is_some() => {
            crate::storage::grant(page, &[json!({"name": "geolocation"})], &origin, Holder::Location).await
        }
        _ => Ok(()),
    }
}

/// Brings a new page or frame session in line with the tab's emulation.
pub async fn on_session(page: &Page, session: &Arc<str>, is_page: bool) {
    let state = page.emulation.lock().expect("emulation lock").clone();
    if state.is_empty() {
        return;
    }
    let blank = State::default();
    let calls = GROUPS
        .into_iter()
        .filter(|group| blank.differs(&state, *group))
        .flat_map(|group| state.commands(group, is_page))
        .map(|(method, params)| page.cdp.send(method, params, Some(session)));
    // A session that fails here is going away; the next one gets the same treatment.
    join_all(calls).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> EmulateArgs {
        EmulateArgs {
            device: None,
            viewport: None,
            dpr: None,
            mobile: false,
            ua: None,
            locale: None,
            tz: None,
            geo: None,
            scheme: None,
            media: None,
            reduced_motion: false,
            network: None,
            cpu: None,
            list: false,
            reset: false,
        }
    }

    #[test]
    fn devices_are_found_loosely_and_suggested() {
        let devices = devices().expect("vendored devices parse");
        assert_eq!(find_device(&devices, "iphone 15").expect("found").0, "iPhone 15");
        assert_eq!(find_device(&devices, "pixel7").expect("found").0, "Pixel 7");
        assert_eq!(find_device(&devices, "Galaxy S9+").expect("found").0, "Galaxy S9+");
        let error = format!("{:#}", find_device(&devices, "iphone").expect_err("ambiguous"));
        assert!(error.contains("did you mean iPhone 11,") && error.contains("more in `hba emulate --list`"), "{error}");
        assert!(!error.contains("landscape"), "landscape variants only when asked: {error}");
        let error = format!("{:#}", find_device(&devices, "toaster").expect_err("unknown"));
        assert!(error.contains("--list"), "{error}");
    }

    #[test]
    fn device_list_folds_landscape_variants() {
        let list = device_list().expect("list");
        assert!(list.contains("iPhone 15,") && !list.contains("iPhone 15 landscape"));
        assert!(list.lines().last().is_some_and(|line| line.contains("landscape")));
    }

    #[test]
    fn network_presets_follow_devtools() {
        let slow = conditions(NetworkPreset::Slow3g).expect("throttled");
        assert_eq!((slow.download, slow.upload, slow.latency_ms), (50_000.0, 50_000.0, 2000.0));
        assert_eq!(conditions(NetworkPreset::Fast3g), conditions(NetworkPreset::Slow4g));
        let fast = conditions(NetworkPreset::Fast3g).expect("throttled");
        assert_eq!((fast.download, fast.upload, fast.latency_ms), (180_000.0, 84_375.0, 562.5));
        assert!(conditions(NetworkPreset::Offline).is_some_and(|c| c.offline));
        assert_eq!(conditions(NetworkPreset::None), None);
    }

    #[test]
    fn devices_set_metrics_and_flags_merge() {
        let devices = devices().expect("devices");
        let (name, device) = find_device(&devices, "Pixel 7").expect("found");
        let state = State::default()
            .merged(&EmulateArgs { locale: Some("fr-FR".into()), ..args() }, Some((name, device)))
            .expect("valid");
        let metrics = state.metrics.expect("metrics");
        assert_eq!((metrics.width, metrics.height, metrics.mobile, metrics.touch), (412, 839, true, true));
        assert_eq!(state.summary(), "emulating Pixel 7, 412x839 @2.625x mobile touch, locale fr-FR");
        let state = state
            .merged(&EmulateArgs { viewport: Some((800, 600)), tz: Some("Asia/Kolkata".into()), ..args() }, None)
            .expect("valid");
        let metrics = state.metrics.expect("metrics");
        assert_eq!((metrics.width, metrics.height, metrics.scale, metrics.screen), (800, 600, 2.625, None));
        assert_eq!(state.locale.as_deref(), Some("fr-FR"), "earlier settings stay");
        assert!(state.merged(&EmulateArgs { cpu: Some(0.5), ..args() }, None).is_err());
        let cleared = state
            .merged(&EmulateArgs { network: Some(NetworkPreset::None), cpu: Some(1.0), ..args() }, None)
            .expect("valid");
        assert_eq!((cleared.network, cleared.cpu), (None, None));
    }

    #[test]
    fn only_changed_groups_are_sent_and_reset_clears_them() {
        let before = State::default();
        let after = before.merged(&EmulateArgs { scheme: Some(ColorScheme::Dark), ..args() }, None).expect("valid");
        let changed: Vec<Group> = GROUPS.into_iter().filter(|g| before.differs(&after, *g)).collect();
        assert_eq!(changed, vec![Group::Media]);
        let commands = after.commands(Group::Media, true);
        assert_eq!(commands[0].1["features"][0]["value"], "dark");
        assert_eq!(State::default().commands(Group::Media, true)[0].1["features"][0]["value"], "");
        assert!(after.commands(Group::Metrics, false).is_empty(), "frames keep the page's metrics");
    }

    #[test]
    fn client_hints_follow_the_user_agent() {
        let android =
            client_hints("Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 Chrome/156.0 Mobile", true);
        assert_eq!((android["platform"].as_str(), android["platformVersion"].as_str()), (Some("Android"), Some("14")));
        let iphone = client_hints("Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X)", true);
        assert_eq!((iphone["platform"].as_str(), iphone["platformVersion"].as_str()), (Some("iOS"), Some("17_5")));
        let mac = client_hints("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)", false);
        assert_eq!((mac["platform"].as_str(), mac["architecture"].as_str()), (Some("macOS"), Some("x86")));
        let windows = client_hints("Mozilla/5.0 (Windows NT 10.0; Win64; x64)", false);
        assert_eq!(
            (windows["platform"].as_str(), windows["platformVersion"].as_str()),
            (Some("Windows"), Some("10.0"))
        );
        let linux = client_hints("Mozilla/5.0 (X11; Linux x86_64)", false);
        assert_eq!(linux["platform"].as_str(), Some("Linux"));
    }
}
