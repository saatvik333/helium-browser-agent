//! Validated configuration from `$XDG_CONFIG_HOME/hba/config.toml` plus `HBA_*` overrides.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};
use std::{env, fs};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// How much human-like timing and motion input gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Humanize {
    /// Human-like on websites; instant on local, file, data and extension pages.
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Helium's user data directory (holds `Local State` and the running Helium's `SingletonLock`).
    pub user_data_dir: PathBuf,
    /// Executable that starts Helium or opens a profile window in the running instance.
    pub launcher: PathBuf,
    /// Profile for new tabs when no flag, environment or workspace rule picks one.
    pub default_profile: Option<String>,
    /// Short names for profiles, e.g. `work = "Modo - Work"`.
    pub aliases: BTreeMap<String, String>,
    /// Directory → profile rules; the longest matching directory wins.
    pub workspaces: Vec<(PathBuf, String)>,
    /// Daemon exits after this long without requests (`None` = never).
    pub idle: Option<Duration>,
    pub humanize: Humanize,
    /// Explicit DevTools WebSocket URL; skips discovery and owner verification.
    pub cdp_url: Option<String>,
    source: Option<(PathBuf, SystemTime)>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    user_data_dir: Option<String>,
    launcher: Option<String>,
    default_profile: Option<String>,
    idle_minutes: Option<u64>,
    humanize: Option<Humanize>,
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    #[serde(default)]
    workspaces: BTreeMap<String, String>,
}

/// Daemon idle lifetime when the config sets none (or does not load).
pub const DEFAULT_IDLE: Duration = Duration::from_secs(30 * 60);

impl Config {
    /// Loads the file named by `HBA_CONFIG`, else the XDG default (absent is fine), then applies overrides.
    pub fn load() -> Result<Self> {
        let explicit = env::var_os("HBA_CONFIG").map(PathBuf::from);
        let path = explicit.clone().unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("hba/config.toml"));
        let (file, source) = match fs::metadata(&path) {
            Ok(meta) => {
                let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
                let file: File = toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))?;
                (file, Some((path, meta.modified()?)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => (File::default(), None),
            Err(error) => return Err(error).with_context(|| format!("reading HBA_CONFIG {}", path.display())),
        };
        Self::from_file(file, source)
    }

    /// True when the config file changed since it was loaded.
    pub fn is_stale(&self) -> bool {
        let current = env::var_os("HBA_CONFIG")
            .map_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("hba/config.toml"), PathBuf::from);
        let modified = fs::metadata(&current).and_then(|meta| meta.modified()).ok();
        match (&self.source, modified) {
            (Some((path, at)), Some(now)) => *path != current || *at != now,
            (None, None) => false,
            _ => true,
        }
    }

    fn from_file(file: File, source: Option<(PathBuf, SystemTime)>) -> Result<Self> {
        // Errors about a key of the file name the file too.
        let at = source.as_ref().map_or_else(String::new, |(path, _)| format!(" in {}", path.display()));
        let user_data_dir = setting("HBA_USER_DATA_DIR", file.user_data_dir, "user_data_dir", &at)?
            .map_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("net.imput.helium"), |dir| expand(&dir));
        let launcher = setting("HBA_LAUNCHER", file.launcher, "launcher", &at)?
            .map_or_else(|| PathBuf::from("helium-browser"), |path| expand(&path));
        let humanize = match env::var("HBA_HUMANIZE") {
            Ok(value) => match value.as_str() {
                "auto" => Humanize::Auto,
                "always" => Humanize::Always,
                "never" => Humanize::Never,
                _ => bail!("HBA_HUMANIZE must be auto, always or never"),
            },
            Err(_) => file.humanize.unwrap_or(Humanize::Auto),
        };
        let cdp_url = env::var("HBA_CDP").ok().filter(|url| !url.is_empty());
        if let Some(url) = &cdp_url
            && !url.starts_with("ws://")
        {
            bail!("HBA_CDP must be a ws:// DevTools URL");
        }
        for (table, entries) in [("aliases", &file.aliases), ("workspaces", &file.workspaces)] {
            for (name, profile) in entries {
                if name.trim().is_empty() || profile.trim().is_empty() {
                    bail!("{table}.{name:?}{at} needs a non-empty name and profile");
                }
            }
        }
        if file.default_profile.as_deref().is_some_and(|name| name.trim().is_empty()) {
            bail!("default_profile{at} must not be empty");
        }
        let mut workspaces = Vec::with_capacity(file.workspaces.len());
        for (dir, profile) in file.workspaces {
            let path = expand(&dir);
            // A relative key would be resolved against the daemon's working directory, not the caller's.
            if !path.is_absolute() {
                bail!("workspaces.{dir:?}{at} must be an absolute path or start with ~/");
            }
            workspaces.push((normalize(&path), profile));
        }
        workspaces.sort_by_key(|(dir, _)| std::cmp::Reverse(dir.components().count()));
        let idle = match file.idle_minutes {
            None => Some(DEFAULT_IDLE),
            Some(0) => None,
            Some(minutes) => Some(Duration::from_secs(
                minutes.checked_mul(60).with_context(|| format!("idle_minutes{at} is too large"))?,
            )),
        };
        Ok(Self {
            user_data_dir,
            launcher,
            default_profile: file.default_profile,
            aliases: file.aliases,
            workspaces,
            idle,
            humanize,
            cdp_url,
            source,
        })
    }

    /// The workspace rule covering `cwd`, if any. Matching is by whole path components.
    pub fn workspace_profile(&self, cwd: &Path) -> Option<&str> {
        let cwd = normalize(cwd);
        self.workspaces.iter().find(|(dir, _)| cwd.starts_with(dir)).map(|(_, profile)| profile.as_str())
    }
}

/// An `HBA_*` variable, else the file's `key`; errors name whichever was set (`at`: the file).
fn setting(var: &str, file: Option<String>, key: &str, at: &str) -> Result<Option<String>> {
    match env::var(var) {
        Ok(value) if value.trim().is_empty() => bail!("{var} must not be empty"),
        Ok(value) => Ok(Some(value)),
        Err(_) => match file {
            Some(value) if value.trim().is_empty() => bail!("{key}{at} must not be empty"),
            other => Ok(other),
        },
    }
}

pub fn xdg(var: &str, fallback: &str) -> PathBuf {
    match env::var_os(var) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(fallback),
    }
}

pub fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// Expands a leading `~/`.
pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if path == "~" => home(),
        None => PathBuf::from(path),
    }
}

/// Canonical form when the path exists, lexically cleaned otherwise.
fn normalize(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other),
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Result<Config> {
        Config::from_file(toml::from_str(text)?, None)
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let error = toml::from_str::<File>("profile = 'x'").unwrap_err().to_string();
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn longest_workspace_wins_on_component_boundaries() {
        let config =
            config("[workspaces]\n'/srv/work' = 'Work'\n'/srv/work/client' = 'Client'\n'/srv/home' = 'Personal'")
                .unwrap();
        assert_eq!(config.workspace_profile(Path::new("/srv/work/client/app")), Some("Client"));
        assert_eq!(config.workspace_profile(Path::new("/srv/work/x")), Some("Work"));
        assert_eq!(config.workspace_profile(Path::new("/srv/work-other")), None);
        assert_eq!(config.workspace_profile(Path::new("/srv/home")), Some("Personal"));
    }

    #[test]
    fn empty_values_are_rejected() {
        assert!(config("default_profile = ' '").is_err());
        assert!(config("[aliases]\nwork = ''").is_err());
    }

    #[test]
    fn idle_zero_means_never() {
        assert_eq!(config("idle_minutes = 0").unwrap().idle, None);
        assert_eq!(config("").unwrap().idle, Some(DEFAULT_IDLE));
        assert_eq!(config("idle_minutes = 2").unwrap().idle, Some(Duration::from_secs(120)));
    }

    #[test]
    fn idle_minutes_must_fit_in_seconds() {
        // Unchecked, this wraps to a daemon that exits after about 44 s.
        let error = config("idle_minutes = 307445734561825861").unwrap_err().to_string();
        assert_eq!(error, "idle_minutes is too large");
    }

    #[test]
    fn relative_workspaces_are_rejected() {
        let error = config("[workspaces]\n'code/work' = 'Work'").unwrap_err().to_string();
        assert_eq!(error, "workspaces.\"code/work\" must be an absolute path or start with ~/");
        assert!(config("[workspaces]\n'~/code/work' = 'Work'").is_ok());
    }

    #[test]
    fn errors_name_the_key_and_the_file() {
        let file = toml::from_str("[aliases]\nwork = ''").unwrap();
        let source = Some((PathBuf::from("/srv/hba.toml"), SystemTime::UNIX_EPOCH));
        let error = Config::from_file(file, source).unwrap_err().to_string();
        assert_eq!(error, "aliases.\"work\" in /srv/hba.toml needs a non-empty name and profile");
    }
}
