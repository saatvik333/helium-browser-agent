//! Named task sessions, their tabs, and short tab numbers, persisted in the runtime directory.
//!
//! State is valid for one browser run: when Helium restarts, target ids change, so tabs and
//! numbers are dropped while session names and profile choices survive.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tab {
    /// Tab target id.
    pub id: String,
    /// The user's tab, taken over with `hba tab use`; released instead of closed.
    #[serde(default)]
    pub adopted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    /// Profile directory chosen for new tabs.
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub tabs: Vec<Tab>,
    #[serde(default)]
    pub current: Option<String>,
}

impl Session {
    pub fn owns(&self, tab: &str) -> bool {
        self.tabs.iter().any(|t| t.id == tab)
    }

    pub fn add(&mut self, tab: &str, adopted: bool) {
        if !self.owns(tab) {
            self.tabs.push(Tab { id: tab.to_owned(), adopted });
        }
    }

    pub fn remove(&mut self, tab: &str) {
        self.tabs.retain(|t| t.id != tab);
        if self.current.as_deref() == Some(tab) {
            self.current = self.tabs.last().map(|t| t.id.clone());
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    /// Browser instance the ids below belong to.
    #[serde(default)]
    pub browser: Option<String>,
    #[serde(default)]
    pub contexts: HashMap<String, String>,
    #[serde(default)]
    numbers: HashMap<String, u32>,
    #[serde(default)]
    next_number: u32,
    #[serde(default)]
    pub sessions: BTreeMap<String, Session>,
    #[serde(skip)]
    path: PathBuf,
}

impl Store {
    /// Loads the store; a damaged file is set aside rather than trusted or silently lost.
    pub fn load(path: PathBuf) -> Self {
        let mut store = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Self>(&bytes).unwrap_or_else(|error| {
                let aside = path.with_extension("json.damaged");
                let _ = std::fs::rename(&path, &aside);
                eprintln!("hba: {} was damaged ({error}); moved to {}", path.display(), aside.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        store.path = path;
        store
    }

    /// Writes the store atomically, readable only by the user.
    pub fn save(&self) -> Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let tmp = self.path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(&serde_json::to_vec(self)?).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("replacing {}", self.path.display()))
    }

    /// Switches to a new browser run, forgetting ids from the previous one.
    pub fn bind_browser(&mut self, browser: &str) {
        if self.browser.as_deref() != Some(browser) {
            self.browser = Some(browser.to_owned());
            self.contexts.clear();
            self.numbers.clear();
            self.next_number = 0;
            for session in self.sessions.values_mut() {
                session.tabs.clear();
                session.current = None;
            }
        }
    }

    pub fn session(&mut self, name: &str) -> &mut Session {
        self.sessions.entry(name.to_owned()).or_default()
    }

    /// Short number of a tab target, assigned on first sight.
    pub fn number(&mut self, tab: &str) -> u32 {
        if let Some(n) = self.numbers.get(tab) {
            return *n;
        }
        self.next_number += 1;
        self.numbers.insert(tab.to_owned(), self.next_number);
        self.next_number
    }

    /// `t5`, `5`, a full target id or a unique id prefix → target id.
    pub fn resolve_tab(&self, query: &str, live: &[String]) -> Result<String> {
        let number = query.strip_prefix('t').unwrap_or(query);
        if let Ok(n) = number.parse::<u32>() {
            return self
                .numbers
                .iter()
                .find(|(_, v)| **v == n)
                .map(|(id, _)| id.clone())
                .with_context(|| format!("no tab t{n}; see `hba tabs --all`"));
        }
        let upper = query.to_ascii_uppercase();
        let matches: Vec<&String> = live.iter().filter(|id| id.starts_with(&upper)).collect();
        match matches.as_slice() {
            [one] => Ok((*one).clone()),
            [] => bail!("no tab {query:?}; see `hba tabs --all`"),
            _ => bail!("tab id prefix {query:?} is ambiguous"),
        }
    }

    /// The session currently holding a tab, if any.
    pub fn holder(&self, tab: &str) -> Option<&str> {
        self.sessions.iter().find(|(_, session)| session.owns(tab)).map(|(name, _)| name.as_str())
    }

    /// Forgets tabs that no longer exist.
    pub fn prune(&mut self, live: &[String]) {
        for session in self.sessions.values_mut() {
            session.tabs.retain(|tab| live.contains(&tab.id));
            if session.current.as_ref().is_some_and(|current| !live.contains(current)) {
                session.current = session.tabs.last().map(|tab| tab.id.clone());
            }
        }
        self.numbers.retain(|id, _| live.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_stable_and_resolvable() {
        let mut store = Store::default();
        assert_eq!(store.number("AAA"), 1);
        assert_eq!(store.number("BBB"), 2);
        assert_eq!(store.number("AAA"), 1);
        let live = vec!["AAA".to_owned(), "BBB".to_owned()];
        assert_eq!(store.resolve_tab("t2", &live).unwrap(), "BBB");
        assert_eq!(store.resolve_tab("1", &live).unwrap(), "AAA");
        assert_eq!(store.resolve_tab("bb", &live).unwrap(), "BBB");
        assert!(store.resolve_tab("t9", &live).is_err());
    }

    #[test]
    fn a_new_browser_run_drops_ids_but_keeps_sessions() {
        let mut store = Store::default();
        store.bind_browser("one");
        store.session("task").profile = Some("Default".into());
        store.session("task").add("AAA", false);
        store.number("AAA");
        store.bind_browser("one");
        assert!(store.sessions["task"].owns("AAA"));
        store.bind_browser("two");
        assert_eq!(store.sessions["task"].tabs.len(), 0);
        assert_eq!(store.sessions["task"].profile.as_deref(), Some("Default"));
        assert_eq!(store.number("BBB"), 1);
    }

    #[test]
    fn removing_the_current_tab_falls_back_to_the_last_one() {
        let mut session = Session::default();
        session.add("A", false);
        session.add("B", true);
        session.current = Some("B".into());
        session.remove("B");
        assert_eq!(session.current.as_deref(), Some("A"));
    }

    #[test]
    fn pruning_forgets_closed_tabs() {
        let mut store = Store::default();
        store.session("s").add("A", false);
        store.session("s").add("B", false);
        store.session("s").current = Some("B".into());
        store.prune(&["A".to_owned()]);
        assert_eq!(store.sessions["s"].tabs, [Tab { id: "A".into(), adopted: false }]);
        assert_eq!(store.sessions["s"].current.as_deref(), Some("A"));
    }
}
