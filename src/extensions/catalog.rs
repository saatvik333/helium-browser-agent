//! Installed extensions of a profile: `Preferences` entries, manifests and EXT resolution.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

/// Where an extension came from (Chromium's `ManifestLocation` in `Preferences`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// From a web store.
    Store,
    /// A `.crx` installed by hand.
    Packed,
    /// Loaded from a directory (`hba ext load`, "Load unpacked", `--load-extension`).
    Unpacked,
    /// Bundled with Helium.
    Component,
    /// Forced by policy.
    Policy,
    /// Installed by other software.
    External,
}

impl Location {
    /// Reads `location` and `from_webstore` of an `extensions.settings` entry.
    fn of(entry: &Value) -> Option<Self> {
        let code = entry.get("location")?.as_i64()?;
        Some(match code {
            1 if entry.get("from_webstore").and_then(Value::as_bool) == Some(true) => Self::Store,
            1 => Self::Packed,
            4 | 8 => Self::Unpacked,
            5 | 10 => Self::Component,
            7 | 9 => Self::Policy,
            _ => Self::External,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Packed => "packed",
            Self::Unpacked => "unpacked",
            Self::Component => "component",
            Self::Policy => "policy",
            Self::External => "external",
        }
    }
}

/// What an extension runs in the background.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Background {
    #[default]
    None,
    /// A Manifest V3 service worker.
    Worker,
    /// A Manifest V2 background or event page.
    Page,
}

/// The parts of `manifest.json` hba acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub background: Background,
    /// Has a toolbar action (`action`, `browser_action` or `page_action`).
    pub action: bool,
    /// The action's popup, relative to the extension root.
    pub popup: Option<String>,
    /// The options page, relative to the extension root.
    pub options: Option<String>,
}

impl Manifest {
    /// Reads `dir/manifest.json`, resolving a localized `__MSG_…__` name.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("manifest.json");
        let text = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let value: Value =
            serde_json::from_slice(&text).with_context(|| format!("{} is not valid JSON", path.display()))?;
        Ok(Self::from_value(&value, Some(dir)))
    }

    /// The fields of manifest JSON; `dir` holds the `_locales` a `__MSG_…__` name refers to.
    pub fn from_value(manifest: &Value, dir: Option<&Path>) -> Self {
        let text = |key: &str| manifest.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let action = ["action", "browser_action", "page_action"].iter().find_map(|key| manifest.get(key));
        let background = match manifest.get("background") {
            Some(background) if background.get("service_worker").is_some() => Background::Worker,
            Some(background) if background.get("page").is_some() || background.get("scripts").is_some() => {
                Background::Page
            }
            _ => Background::None,
        };
        let page =
            |value: Option<&Value>| value.and_then(Value::as_str).filter(|page| !page.is_empty()).map(str::to_owned);
        let mut name = text("name");
        if let (Some(key), Some(dir)) = (message_key(&name), dir)
            && let Some(localized) = localized(dir, manifest, key)
        {
            name = localized;
        }
        Self {
            name,
            version: text("version"),
            background,
            action: action.is_some(),
            popup: page(action.and_then(|action| action.get("default_popup"))),
            options: page(manifest.pointer("/options_ui/page").or_else(|| manifest.get("options_page"))),
        }
    }
}

/// `key` of a `__MSG_key__` placeholder.
fn message_key(text: &str) -> Option<&str> {
    text.strip_prefix("__MSG_")?.strip_suffix("__")
}

/// A message from `_locales/<locale>/messages.json`: the locale Helium chose, else the default.
fn localized(dir: &Path, manifest: &Value, key: &str) -> Option<String> {
    let mut locales =
        ["current_locale", "default_locale"].into_iter().filter_map(|field| manifest.get(field)?.as_str());
    locales.find_map(|locale| {
        let text = fs::read(dir.join("_locales").join(locale).join("messages.json")).ok()?;
        let messages: Map<String, Value> = serde_json::from_slice(&text).ok()?;
        let (_, message) = messages.iter().find(|(name, _)| name.eq_ignore_ascii_case(key))?;
        message.get("message")?.as_str().map(str::to_owned)
    })
}

/// An installed extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub id: String,
    pub manifest: Manifest,
    pub enabled: bool,
    pub location: Location,
    /// The extension's files; for unpacked extensions the directory they were loaded from.
    pub dir: Option<PathBuf>,
}

impl Extension {
    pub fn name(&self) -> &str {
        if self.manifest.name.is_empty() { &self.id } else { &self.manifest.name }
    }

    /// `chrome-extension://ID/`, the prefix of every URL the extension serves.
    pub fn origin(&self) -> String {
        origin(&self.id)
    }

    /// The URL of `page` (relative to the extension root, or already a full URL).
    pub fn url(&self, page: &str) -> String {
        if page.contains("://") {
            return page.to_owned();
        }
        let path = page.trim_start_matches("./").trim_start_matches('/');
        format!("{}{path}", self.origin())
    }
}

/// `chrome-extension://ID/`.
pub fn origin(id: &str) -> String {
    format!("chrome-extension://{id}/")
}

/// An unpacked extension as `Extensions.getExtensions` reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct Loaded {
    pub id: String,
    pub name: String,
    pub version: String,
    pub path: PathBuf,
    pub enabled: bool,
}

/// Extensions recorded in a profile's `Secure Preferences` and `Preferences`, sorted by name.
/// Helium's hidden components (no toolbar action) are left out, as `chrome://extensions` does.
pub fn installed(profile_dir: &Path) -> Result<Vec<Extension>> {
    let mut settings = Map::new();
    for file in ["Secure Preferences", "Preferences"] {
        let path = profile_dir.join(file);
        let text = match fs::read(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let prefs: Value = serde_json::from_slice(&text).with_context(|| format!("parsing {}", path.display()))?;
        merge_settings(&mut settings, &prefs);
    }
    let mut list: Vec<Extension> =
        settings.iter().filter_map(|(id, entry)| parse_entry(id, entry, profile_dir)).collect();
    sort(&mut list);
    Ok(list)
}

/// Adds `extensions.settings` of one preferences file; an extension's fields may be split
/// between the two files.
fn merge_settings(settings: &mut Map<String, Value>, prefs: &Value) {
    let Some(entries) = prefs.pointer("/extensions/settings").and_then(Value::as_object) else {
        return;
    };
    for (id, entry) in entries {
        let Some(fields) = entry.as_object() else {
            continue;
        };
        let slot = settings.entry(id.clone()).or_insert_with(|| Value::Object(Map::new()));
        if let Some(slot) = slot.as_object_mut() {
            slot.extend(fields.iter().map(|(key, value)| (key.clone(), value.clone())));
        }
    }
}

fn parse_entry(id: &str, entry: &Value, profile_dir: &Path) -> Option<Extension> {
    let location = Location::of(entry)?;
    let dir = entry.get("path").and_then(Value::as_str).filter(|path| !path.is_empty()).map(|path| {
        let path = Path::new(path);
        // Store installs are recorded relative to the profile's `Extensions` directory.
        if path.is_absolute() { path.to_path_buf() } else { profile_dir.join("Extensions").join(path) }
    });
    // Unpacked extensions keep no manifest copy in the preferences: Helium rereads the directory.
    let manifest = match entry.get("manifest") {
        Some(manifest) => Manifest::from_value(manifest, dir.as_deref()),
        None => dir.as_deref().and_then(|dir| Manifest::load(dir).ok()).unwrap_or_default(),
    };
    if location == Location::Component && !manifest.action {
        return None;
    }
    Some(Extension { id: id.to_owned(), manifest, enabled: !disabled(entry), location, dir })
}

/// Whether an entry is disabled: `disable_reasons` (a list, formerly a bit mask) or the old `state`.
fn disabled(entry: &Value) -> bool {
    let reasons = match entry.get("disable_reasons") {
        Some(Value::Array(reasons)) => !reasons.is_empty(),
        Some(Value::Number(mask)) => mask.as_i64() != Some(0),
        _ => false,
    };
    reasons || entry.get("state").and_then(Value::as_i64) == Some(0)
}

/// Replaces the unpacked extensions with the ones the browser reports: `Preferences` is written
/// seconds after a load or removal, the browser knows immediately.
pub fn merge_loaded(list: &mut Vec<Extension>, loaded: Vec<Loaded>) {
    list.retain(|known| known.location != Location::Unpacked || loaded.iter().any(|item| item.id == known.id));
    for item in loaded {
        let mut manifest = Manifest::load(&item.path).unwrap_or_default();
        manifest.name = item.name;
        manifest.version = item.version;
        let extension = Extension {
            id: item.id,
            manifest,
            enabled: item.enabled,
            location: Location::Unpacked,
            dir: Some(item.path),
        };
        match list.iter_mut().find(|known| known.id == extension.id) {
            Some(known) => *known = extension,
            None => list.push(extension),
        }
    }
    sort(list);
}

fn sort(list: &mut [Extension]) {
    list.sort_by_cached_key(|extension| (extension.name().to_lowercase(), extension.id.clone()));
}

/// Whether `text` is an extension id: 32 letters `a`–`p`.
pub fn is_id(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|byte| (b'a'..=b'p').contains(&byte))
}

/// The one extension `query` names: an id, the unpacked directory `dir` (the query resolved as
/// a path, when it looks like one), the exact name, or a unique part of a name (any case).
pub fn resolve<'a>(list: &'a [Extension], query: &str, dir: Option<&Path>) -> Result<&'a Extension> {
    if is_id(query) {
        return list
            .iter()
            .find(|extension| extension.id == query)
            .with_context(|| format!("no extension {query} in this profile; `hba ext` lists them"));
    }
    if let Some(dir) = dir {
        let wanted = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let found = list.iter().find(|extension| {
            extension.location == Location::Unpacked
                && extension
                    .dir
                    .as_deref()
                    .is_some_and(|loaded| fs::canonicalize(loaded).unwrap_or_else(|_| loaded.to_path_buf()) == wanted)
        });
        return found.with_context(|| {
            format!("no extension is loaded from {}; `hba ext load {}` installs it", wanted.display(), wanted.display())
        });
    }
    let lower = query.to_lowercase();
    let tiers: [&dyn Fn(&Extension) -> bool; 2] =
        [&|extension| extension.name().to_lowercase() == lower, &|extension| {
            extension.name().to_lowercase().contains(&lower)
        }];
    for matches in tiers {
        let found: Vec<&Extension> = list.iter().filter(|extension| matches(extension)).collect();
        match found.as_slice() {
            [one] => return Ok(one),
            [] => {}
            many => bail!(
                "{query:?} matches several extensions: {}; use an id",
                many.iter()
                    .map(|extension| format!("{} ({})", extension.name(), extension.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    bail!("no extension matches {query:?}; `hba ext` lists them")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const UNPACKED: &str = "ilidgonjdoemebfgciljbbklchfokkln";
    const STORE: &str = "cjpalhdlnbpafiamejdnhcphjbkeiagm";
    const UBLOCK: &str = "blockjmkbacgjkknlgpkjjiijinjdanf";
    const PDF: &str = "mhjfbmdgcfjbbpaeojofohoefgiehjai";

    /// A scratch directory under the system temp dir, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("hba-catalog-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("creating scratch dir");
            Self(dir)
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().expect("file has a parent")).expect("creating parent dirs");
            fs::write(path, text).expect("writing scratch file");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn extension(id: &str, name: &str, location: Location, dir: Option<&str>) -> Extension {
        Extension {
            id: id.to_owned(),
            manifest: Manifest { name: name.to_owned(), version: "1.0".to_owned(), ..Manifest::default() },
            enabled: true,
            location,
            dir: dir.map(PathBuf::from),
        }
    }

    #[test]
    fn manifest_v3_fields() {
        let manifest = Manifest::from_value(
            &json!({
                "name": "Fixture", "version": "1.2.3",
                "background": {"service_worker": "sw.js"},
                "action": {"default_popup": "popup.html"},
                "options_ui": {"page": "options.html"}
            }),
            None,
        );
        assert_eq!(
            manifest,
            Manifest {
                name: "Fixture".to_owned(),
                version: "1.2.3".to_owned(),
                background: Background::Worker,
                action: true,
                popup: Some("popup.html".to_owned()),
                options: Some("options.html".to_owned()),
            }
        );
    }

    #[test]
    fn manifest_v2_fields() {
        let manifest = Manifest::from_value(
            &json!({
                "name": "Old", "version": "2",
                "background": {"scripts": ["bg.js"], "persistent": false},
                "browser_action": {"default_title": "no popup"},
                "options_page": "settings.html"
            }),
            None,
        );
        assert_eq!(manifest.background, Background::Page);
        assert!(manifest.action, "browser_action counts as a toolbar action");
        assert_eq!(manifest.popup, None);
        assert_eq!(manifest.options.as_deref(), Some("settings.html"));
        let bare = Manifest::from_value(
            &json!({"name": "Content only", "version": "1", "action": {"default_popup": ""}}),
            None,
        );
        assert_eq!((bare.background, bare.popup), (Background::None, None), "an empty popup is no popup");
    }

    #[test]
    fn localized_name_prefers_the_chosen_locale() {
        let scratch = Scratch::new("locale");
        scratch.write("_locales/en/messages.json", r#"{"extName": {"message": "English name"}}"#);
        scratch.write("_locales/de/messages.json", r#"{"EXTNAME": {"message": "Deutscher Name"}}"#);
        let manifest = json!({"name": "__MSG_extName__", "version": "1", "default_locale": "en"});
        assert_eq!(Manifest::from_value(&manifest, Some(&scratch.0)).name, "English name");
        let chosen = json!({"name": "__MSG_extName__", "version": "1", "default_locale": "en", "current_locale": "de"});
        assert_eq!(Manifest::from_value(&chosen, Some(&scratch.0)).name, "Deutscher Name", "keys match in any case");
        let missing = json!({"name": "__MSG_other__", "version": "1", "default_locale": "en"});
        assert_eq!(Manifest::from_value(&missing, Some(&scratch.0)).name, "__MSG_other__", "unknown keys stay visible");
    }

    #[test]
    fn preferences_entries() {
        let profile = Scratch::new("prefs");
        let unpacked = Scratch::new("prefs-unpacked");
        unpacked
            .write("manifest.json", r#"{"name": "Mine", "version": "0.1", "background": {"service_worker": "sw.js"}}"#);
        let unpacked_dir = unpacked.0.to_str().expect("utf-8 temp path");
        let prefs = json!({"extensions": {"settings": {
            UNPACKED: {"location": 4, "path": unpacked_dir, "disable_reasons": []},
            STORE: {"location": 1, "from_webstore": true, "path": format!("{STORE}/3.0_0"), "disable_reasons": [1],
                    "manifest": {"name": "Store one", "version": "3.0"}},
            UBLOCK: {"location": 5, "path": "/opt/helium/ublock",
                     "manifest": {"name": "uBlock Origin", "version": "1.75", "browser_action": {}}},
            PDF: {"location": 5, "path": "/opt/helium/pdf", "manifest": {"name": "PDF viewer", "version": "1"}},
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": {"preferences": {}}
        }}});
        profile.write("Preferences", &prefs.to_string());
        profile.write("Secure Preferences", &json!({"extensions": {"settings": {STORE: {"state": 1}}}}).to_string());
        let list = installed(&profile.0).expect("preferences parse");
        let summary: Vec<(&str, &str, Location, bool)> =
            list.iter().map(|e| (e.id.as_str(), e.name(), e.location, e.enabled)).collect();
        assert_eq!(
            summary,
            [
                (UNPACKED, "Mine", Location::Unpacked, true),
                (STORE, "Store one", Location::Store, false),
                (UBLOCK, "uBlock Origin", Location::Component, true),
            ],
            "hidden components and entries without a location are left out"
        );
        assert_eq!(list[0].manifest.background, Background::Worker, "unpacked manifests are read from disk");
        assert_eq!(
            list[1].dir,
            Some(profile.0.join("Extensions").join(STORE).join("3.0_0")),
            "store paths are profile-relative"
        );
    }

    #[test]
    fn disabled_entries() {
        assert!(!disabled(&json!({"disable_reasons": []})));
        assert!(disabled(&json!({"disable_reasons": [1]})));
        assert!(disabled(&json!({"disable_reasons": 4})));
        assert!(!disabled(&json!({"disable_reasons": 0, "state": 1})));
        assert!(disabled(&json!({"state": 0})));
    }

    #[test]
    fn loaded_extensions_replace_unpacked_preferences() {
        const REMOVED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut list = vec![
            extension(UNPACKED, "Old name", Location::Unpacked, Some("/nowhere/old")),
            extension(REMOVED, "Removed a moment ago", Location::Unpacked, Some("/nowhere/gone")),
            extension(UBLOCK, "uBlock Origin", Location::Component, None),
        ];
        let loaded = vec![
            Loaded {
                id: UNPACKED.to_owned(),
                name: "New name".to_owned(),
                version: "2.0".to_owned(),
                path: "/nowhere/new".into(),
                enabled: true,
            },
            Loaded {
                id: STORE.to_owned(),
                name: "Another".to_owned(),
                version: "1".to_owned(),
                path: "/nowhere/other".into(),
                enabled: false,
            },
        ];
        merge_loaded(&mut list, loaded);
        let summary: Vec<(&str, &str, bool)> =
            list.iter().map(|e| (e.name(), e.manifest.version.as_str(), e.enabled)).collect();
        assert_eq!(
            summary,
            [("Another", "1", false), ("New name", "2.0", true), ("uBlock Origin", "1.0", true)],
            "unpacked entries the browser no longer lists are gone; others stay"
        );
    }

    #[test]
    fn resolve_tiers() {
        let list = vec![
            extension(UNPACKED, "hba test extension", Location::Unpacked, Some("/nowhere/fixture")),
            extension(STORE, "hba test extension helper", Location::Store, None),
            extension(UBLOCK, "uBlock Origin", Location::Component, None),
        ];
        let name = |query: &str, dir: Option<&str>| resolve(&list, query, dir.map(Path::new)).map(|e| e.id.as_str());
        assert_eq!(name(UBLOCK, None).ok(), Some(UBLOCK), "id");
        assert_eq!(name("HBA TEST EXTENSION", None).ok(), Some(UNPACKED), "an exact name beats substrings");
        assert_eq!(name("block", None).ok(), Some(UBLOCK), "unique substring");
        assert_eq!(name("./fixture", Some("/nowhere/fixture")).ok(), Some(UNPACKED), "unpacked directory");
        let ambiguous = name("hba", None).expect_err("two names contain hba").to_string();
        assert!(ambiguous.contains(UNPACKED) && ambiguous.contains(STORE), "{ambiguous}");
        assert!(name("nothing", None).is_err());
        assert!(name(PDF, None).is_err(), "unknown id");
        assert!(name("./elsewhere", Some("/nowhere/elsewhere")).is_err(), "a directory nothing was loaded from");
    }

    #[test]
    fn page_urls() {
        let fixture = extension(UNPACKED, "x", Location::Unpacked, None);
        assert_eq!(fixture.url("popup.html"), format!("chrome-extension://{UNPACKED}/popup.html"));
        assert_eq!(fixture.url("/pages/a.html?x=1"), format!("chrome-extension://{UNPACKED}/pages/a.html?x=1"));
        assert_eq!(fixture.url("./b.html"), format!("chrome-extension://{UNPACKED}/b.html"));
        assert_eq!(fixture.url("chrome-extension://other/c.html"), "chrome-extension://other/c.html");
        assert!(is_id(UNPACKED) && !is_id("uBlock") && !is_id(&"z".repeat(32)));
    }
}
