//! Client-daemon protocol: one JSON request line, one JSON reply line, over a private Unix socket.

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs, io};

use serde::{Deserialize, Serialize};

use crate::cli::Command;

/// Identifies the exact build so a client never talks to a daemon from another build.
pub const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("HBA_BUILD_ID"));

/// Hidden first argument that turns the binary into the daemon.
pub const DAEMON_ARG: &str = "__hba-daemon";

/// How long a daemon that is shutting down may finish its commands. It holds `daemon.lock`
/// until then, and the daemon replacing it waits for that lock.
pub const DAEMON_DRAIN: Duration = Duration::from_secs(60);

/// Variables the daemon reads once when it starts. A client whose values differ gets a fresh
/// daemon instead of one that would silently ignore them.
const DAEMON_ENV: [&str; 7] =
    ["HBA_CONFIG", "HBA_USER_DATA_DIR", "HBA_LAUNCHER", "HBA_HUMANIZE", "HBA_CDP", "HBA_TRACE", "XDG_CONFIG_HOME"];

/// This process's values of [`DAEMON_ENV`], as one comparable string.
pub fn daemon_env() -> String {
    DAEMON_ENV.iter().filter_map(|name| Some(format!("{name}={}\n", env::var_os(name)?.to_string_lossy()))).collect()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub build: String,
    /// The client's [`daemon_env`].
    pub env: String,
    pub cwd: PathBuf,
    pub session: Option<String>,
    pub profile: Option<String>,
    pub tab: Option<String>,
    pub json: bool,
    pub timeout_secs: Option<f64>,
    pub command: Command,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    /// Normal completion; `code` is the process exit status.
    Done { stdout: String, stderr: String, code: u8 },
    /// The daemon is from another build or environment and is exiting; start a fresh one and retry.
    Restart,
}

/// Private per-user runtime directory (`$XDG_RUNTIME_DIR/hba`), created with mode 0700.
pub fn runtime_dir() -> io::Result<PathBuf> {
    let uid = fs::metadata("/proc/self")?.uid();
    let base = match env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            // A shared temporary directory: another user could have created the path first.
            let base = env::temp_dir().join(format!("hba-{uid}"));
            if let Err(error) = fs::DirBuilder::new().mode(0o700).create(&base)
                && error.kind() != io::ErrorKind::AlreadyExists
            {
                return Err(error);
            }
            let meta = fs::symlink_metadata(&base)?;
            if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::other(format!(
                    "{} must be a directory of yours with mode 0700; remove it or set XDG_RUNTIME_DIR",
                    base.display()
                )));
            }
            base
        }
    };
    let dir = base.join("hba");
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid {
        return Err(io::Error::other(format!("{} is not a directory of yours", dir.display())));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

pub fn socket_path(runtime: &Path) -> PathBuf {
    runtime.join("hba.sock")
}
