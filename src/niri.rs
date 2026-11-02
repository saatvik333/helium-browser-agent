//! Best-effort niri integration: focus restoration when a profile window must be opened, and
//! whether Helium's window is on screen.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

/// Longest wait for `niri msg`; the daemon must not hang on a stuck compositor.
const MSG_TIMEOUT: Duration = Duration::from_secs(1);

/// Runs `niri msg ARGS` and returns its standard output when it succeeds in time.
async fn msg(args: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new("niri");
    command.arg("msg").args(args).stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    if std::env::var_os("NIRI_SOCKET").is_none() {
        command.env("NIRI_SOCKET", socket()?);
    }
    let output = tokio::time::timeout(MSG_TIMEOUT, command.output()).await.ok()?.ok()?;
    output.status.success().then_some(output.stdout)
}

/// `niri msg -j WHAT`, parsed.
async fn query(what: &str) -> Option<Value> {
    serde_json::from_slice(&msg(&["-j", what]).await?).ok()
}

/// For a process started outside the session environment (no `NIRI_SOCKET`): the socket of the
/// running niri, `$XDG_RUNTIME_DIR/niri.WAYLAND_DISPLAY.PID.sock`.
fn socket() -> Option<PathBuf> {
    let dir = std::fs::read_dir(std::env::var_os("XDG_RUNTIME_DIR")?).ok()?;
    dir.flatten().map(|entry| entry.path()).find(|path| {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else { return false };
        let Some(pid) = name.strip_prefix("niri.").and_then(|rest| rest.strip_suffix(".sock")) else { return false };
        let pid = pid.rsplit('.').next().unwrap_or_default();
        std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.trim_end() == "niri")
    })
}

/// The focused niri window id, if niri is running.
pub async fn focused_window() -> Option<u64> {
    query("focused-window").await?.get("id")?.as_u64()
}

/// Whether process `pid`'s windows are on active niri workspaces: `Some(true)` when all are,
/// `Some(false)` when none is, `None` when only some are (which one shows a given tab is not
/// known), without niri, or when the process has no window.
pub async fn shows_process(pid: u32) -> Option<bool> {
    let windows = query("windows").await?;
    let workspaces: Vec<u64> = windows
        .as_array()?
        .iter()
        .filter(|window| window.get("pid").and_then(Value::as_u64) == Some(u64::from(pid)))
        .filter_map(|window| window.get("workspace_id")?.as_u64())
        .collect();
    if workspaces.is_empty() {
        return None;
    }
    let active: Vec<u64> = query("workspaces")
        .await?
        .as_array()?
        .iter()
        .filter(|workspace| workspace.get("is_active").and_then(Value::as_bool) == Some(true))
        .filter_map(|workspace| workspace.get("id")?.as_u64())
        .collect();
    on_screen(&workspaces, &active)
}

/// Whether windows on `workspaces` are shown, given the `active` workspaces: all, none, or
/// unknown (only some, or no windows).
fn on_screen(workspaces: &[u64], active: &[u64]) -> Option<bool> {
    match workspaces.iter().filter(|id| active.contains(id)).count() {
        _ if workspaces.is_empty() => None,
        0 => Some(false),
        shown if shown == workspaces.len() => Some(true),
        _ => None,
    }
}

/// Refocuses `previous` when Helium took focus while opening a window.
pub async fn restore_focus(previous: Option<u64>) {
    let Some(previous) = previous else { return };
    if focused_window().await != Some(previous) {
        msg(&["action", "focus-window", "--id", &previous.to_string()]).await;
    }
}

#[cfg(test)]
mod tests {
    use super::on_screen;

    #[test]
    fn several_windows_leave_it_unknown() {
        assert_eq!(on_screen(&[1, 1], &[1, 4]), Some(true));
        assert_eq!(on_screen(&[2], &[1, 4]), Some(false));
        assert_eq!(on_screen(&[1, 2], &[1, 4]), None, "one profile's window shown, another's hidden");
        assert_eq!(on_screen(&[], &[1]), None);
    }
}
