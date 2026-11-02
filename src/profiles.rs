//! Browser profiles from `Local State`: display names, directories and which are open.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Directory under the user data dir, e.g. `Default` or `Profile 3`.
    pub dir: String,
    /// Display name, e.g. `Personal`.
    pub name: String,
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Profiles {
    /// Sorted by directory.
    pub list: Vec<Profile>,
    /// Directory of the profile the browser used last.
    pub last_used: Option<String>,
    /// Directories the browser had open when it last saved its state.
    pub open: Vec<String>,
}

#[derive(Deserialize)]
struct LocalState {
    #[serde(default)]
    profile: Section,
}

#[derive(Default, Deserialize)]
struct Section {
    #[serde(default)]
    info_cache: BTreeMap<String, Entry>,
    last_used: Option<String>,
    #[serde(default)]
    last_active_profiles: Vec<String>,
}

#[derive(Deserialize)]
struct Entry {
    name: Option<String>,
}

impl Profiles {
    /// Reads `Local State`. A brand-new data dir (no file yet, or no profiles recorded) has
    /// exactly Chromium's `Default` profile.
    pub fn load(user_data_dir: &Path) -> Result<Self> {
        let path = user_data_dir.join("Local State");
        match std::fs::read(&path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("parsing {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && user_data_dir.is_dir() => Ok(Self::fresh()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn parse(json: &[u8]) -> Result<Self> {
        let state: LocalState = serde_json::from_slice(json)?;
        let list: Vec<Profile> = state
            .profile
            .info_cache
            .into_iter()
            .map(|(dir, entry)| Profile { name: entry.name.unwrap_or_else(|| dir.clone()), dir })
            .collect();
        if list.is_empty() {
            return Ok(Self::fresh());
        }
        Ok(Self { list, last_used: state.profile.last_used, open: state.profile.last_active_profiles })
    }

    fn fresh() -> Self {
        let dir = "Default".to_owned();
        Self { list: vec![Profile { name: dir.clone(), dir: dir.clone() }], last_used: Some(dir), open: Vec::new() }
    }

    pub fn by_dir(&self, dir: &str) -> Option<&Profile> {
        self.list.iter().find(|profile| profile.dir == dir)
    }

    /// Resolves an alias, display name, directory, or unambiguous case-insensitive part of a name.
    pub fn resolve(&self, query: &str, aliases: &BTreeMap<String, String>) -> Result<&Profile> {
        let query = query.trim();
        let wanted = aliases
            .iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(query))
            .map_or(query, |(_, name)| name.as_str());
        let lower = wanted.to_lowercase();
        let tiers: [&dyn Fn(&Profile) -> bool; 4] = [
            &|p| p.name == wanted,
            &|p| p.dir == wanted,
            &|p| p.name.to_lowercase() == lower || p.dir.to_lowercase() == lower,
            &|p| p.name.to_lowercase().contains(&lower),
        ];
        for matches in tiers {
            let found: Vec<&Profile> = self.list.iter().filter(|p| matches(p)).collect();
            match found.as_slice() {
                [one] => return Ok(one),
                [] => {}
                many => bail!(
                    "profile {query:?} is ambiguous: {}",
                    many.iter().map(|p| format!("{} ({})", p.name, p.dir)).collect::<Vec<_>>().join(", ")
                ),
            }
        }
        bail!(
            "no profile matches {query:?}; profiles: {}",
            self.list.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        )
    }

    /// The profile for new tabs: the first of an explicit choice (`-p`), the session's earlier
    /// profile, a workspace or default rule, and the browser's last-used profile.
    pub fn choose(
        &self,
        flag: Option<&str>,
        session: Option<&str>,
        rule: Option<&str>,
        aliases: &BTreeMap<String, String>,
    ) -> Result<&Profile> {
        if let Some(query) = flag {
            return self.resolve(query, aliases);
        }
        if let Some(profile) = session.and_then(|dir| self.by_dir(dir)) {
            return Ok(profile);
        }
        if let Some(query) = rule {
            return self.resolve(query, aliases);
        }
        self.last_used
            .as_deref()
            .and_then(|dir| self.by_dir(dir))
            .or(self.list.first())
            .context("the browser has no profiles")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = r#"{"profile":{"info_cache":{
        "Default":{"name":"Personal"},"Profile 3":{"name":"Modo - Work"},
        "Profile 5":{"name":"Testing"},"Profile 6":{"name":"Test Two"}},
        "last_used":"Default","last_active_profiles":["Default"]}}"#;

    fn profiles() -> Profiles {
        Profiles::parse(STATE.as_bytes()).unwrap()
    }

    #[test]
    fn resolution_prefers_exact_then_directory_then_case_then_substring() {
        let p = profiles();
        let none = BTreeMap::new();
        assert_eq!(p.resolve("Personal", &none).unwrap().dir, "Default");
        assert_eq!(p.resolve("Profile 3", &none).unwrap().name, "Modo - Work");
        assert_eq!(p.resolve("personal", &none).unwrap().dir, "Default");
        assert_eq!(p.resolve("work", &none).unwrap().dir, "Profile 3");
        assert!(p.resolve("test", &none).unwrap_err().to_string().contains("ambiguous"));
        assert!(p.resolve("nope", &none).unwrap_err().to_string().contains("profiles: "));
    }

    #[test]
    fn aliases_map_to_display_names() {
        let aliases = BTreeMap::from([("w".to_owned(), "Modo - Work".to_owned())]);
        assert_eq!(profiles().resolve("W", &aliases).unwrap().dir, "Profile 3");
    }

    #[test]
    fn choice_order_is_flag_session_rule_then_last_used() {
        let p = profiles();
        let none = BTreeMap::new();
        let dir = |flag, session, rule| p.choose(flag, session, rule, &none).unwrap().dir.clone();
        assert_eq!(dir(Some("Testing"), Some("Profile 3"), Some("Personal")), "Profile 5");
        assert_eq!(dir(None, Some("Profile 3"), Some("Personal")), "Profile 3");
        assert_eq!(dir(None, Some("Profile 9"), Some("Test Two")), "Profile 6");
        assert_eq!(dir(None, None, None), "Default");
        assert!(p.choose(Some("nope"), Some("Profile 3"), None, &none).is_err());
    }

    #[test]
    fn open_and_last_used_are_read() {
        let p = profiles();
        assert_eq!(p.last_used.as_deref(), Some("Default"));
        assert_eq!(p.open, ["Default"]);
        assert_eq!(p.list.len(), 4);
    }
}
