//! The system clipboard, through the desktop's clipboard tools (wl-clipboard, xclip or xsel).

use std::io::ErrorKind;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::cli::ClipArgs;
use crate::daemon::Cx;

/// Longest a clipboard tool may take; they answer in milliseconds unless the display hangs.
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// A clipboard tool's commands for reading and writing text.
#[derive(Debug, PartialEq, Eq)]
struct Tool {
    read: &'static [&'static str],
    write: &'static [&'static str],
}

const WL_CLIPBOARD: Tool = Tool {
    read: &["wl-paste", "--no-newline", "--type", "text"],
    write: &["wl-copy", "--type", "text/plain;charset=utf-8"],
};
const XCLIP: Tool =
    Tool { read: &["xclip", "-selection", "clipboard", "-out"], write: &["xclip", "-selection", "clipboard", "-in"] };
const XSEL: Tool = Tool { read: &["xsel", "--clipboard", "--output"], write: &["xsel", "--clipboard", "--input"] };

/// Tools that can reach the session's clipboard, preferred first.
fn tools(wayland: bool, x11: bool) -> Vec<&'static Tool> {
    let mut tools = Vec::with_capacity(3);
    if wayland {
        tools.push(&WL_CLIPBOARD);
    }
    if x11 || wayland {
        // Wayland desktops usually run Xwayland, whose clipboard they mirror.
        tools.extend([&XCLIP, &XSEL]);
    }
    tools
}

/// Whether a failed read only means the clipboard holds nothing (or no text).
fn is_empty_clipboard(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("nothing is copied") || stderr.contains("no suitable type") || stderr.contains("not available")
}

pub async fn clip(cx: &mut Cx<'_>, args: ClipArgs) -> Result<()> {
    let has = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    let tools = tools(has("WAYLAND_DISPLAY"), has("DISPLAY"));
    if tools.is_empty() {
        bail!(
            "the hba daemon sees no desktop session (WAYLAND_DISPLAY and DISPLAY are unset); run `hba daemon stop`, then rerun from a desktop terminal"
        );
    }
    let mut failure = None;
    for tool in tools {
        let outcome = match &args.text {
            Some(text) => {
                write(tool, text, TOOL_TIMEOUT).await.map(|()| format!("copied {} characters", text.chars().count()))
            }
            None => read(tool).await,
        };
        match outcome {
            Ok(line) => {
                cx.say(line);
                return Ok(());
            }
            Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == ErrorKind::NotFound) => {}
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => bail!("no clipboard tool is installed: install wl-clipboard (Wayland), or xclip or xsel (X11)"),
    }
}

async fn read(tool: &Tool) -> Result<String> {
    let (program, flags) = tool.read.split_first().expect("tool commands are not empty");
    let child = Command::new(program)
        .args(flags)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let output = tokio::time::timeout(TOOL_TIMEOUT, child.wait_with_output())
        .await
        .with_context(|| format!("{program} did not answer within {}s", TOOL_TIMEOUT.as_secs()))??;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        if is_empty_clipboard(&stderr) {
            return Ok("(the clipboard holds no text)".to_owned());
        }
        bail!("{program} failed: {}", stderr.trim());
    }
    match String::from_utf8(output.stdout) {
        Ok(text) if text.is_empty() => Ok("(the clipboard is empty)".to_owned()),
        Ok(text) => Ok(text),
        Err(_) => Ok("(the clipboard holds binary data, not text)".to_owned()),
    }
}

/// Copies `text` with `tool`, which must take it and finish within `timeout`.
async fn write(tool: &Tool, text: &str, timeout: Duration) -> Result<()> {
    let (program, flags) = tool.write.split_first().expect("tool commands are not empty");
    // The tools fork a process that serves the clipboard and inherits stdout and stderr, so
    // stdout is discarded and stderr is read only when the tool failed (and did not fork).
    let mut child = Command::new(program)
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("the clipboard tool has no stdin")?;
    // A tool stuck connecting to the display stops reading once the pipe is full.
    let deadline = tokio::time::Instant::now() + timeout;
    let written = tokio::time::timeout_at(deadline, stdin.write_all(text.as_bytes())).await;
    drop(stdin);
    let status = match written {
        Ok(result) => {
            result.with_context(|| format!("writing to {program}"))?;
            tokio::time::timeout_at(deadline, child.wait()).await
        }
        Err(elapsed) => Err(elapsed),
    };
    let Ok(status) = status else {
        child.kill().await.ok();
        bail!("{program} did not finish within {}s", timeout.as_secs_f32());
    };
    let status = status?;
    if status.success() {
        return Ok(());
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = tokio::time::timeout(Duration::from_millis(200), pipe.read_to_string(&mut stderr)).await;
    }
    bail!("{program} failed: {}", stderr.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_prefers_wl_clipboard_and_x11_skips_it() {
        assert_eq!(tools(true, true), vec![&WL_CLIPBOARD, &XCLIP, &XSEL]);
        assert_eq!(tools(false, true), vec![&XCLIP, &XSEL]);
        assert_eq!(tools(false, false), Vec::<&Tool>::new());
    }

    #[test]
    fn empty_clipboards_are_not_errors() {
        assert!(is_empty_clipboard("Nothing is copied\n"));
        assert!(is_empty_clipboard("No suitable type of content copied"));
        assert!(is_empty_clipboard("Error: target STRING not available"));
        assert!(!is_empty_clipboard("Failed to connect to a Wayland server"));
    }

    #[tokio::test]
    async fn a_tool_that_takes_no_text_is_stopped_in_time() {
        // `sleep` never reads stdin, so more text than a pipe holds blocks the write.
        let stuck = Tool { read: &[], write: &["sleep", "30"] };
        let started = std::time::Instant::now();
        let error = write(&stuck, &"x".repeat(1 << 20), Duration::from_millis(300)).await.expect_err("stuck");
        assert!(error.to_string().contains("did not finish within 0.3s"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    }
}
