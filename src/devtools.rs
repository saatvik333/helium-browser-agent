//! Developer tools for agents: console, errors, the network log and interception, performance
//! tools, and the browser's own developer tools window.

mod coverage;
mod har;
mod logs;
mod net;
mod panel;
mod perf;
mod vitals;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::daemon::Cx;
use crate::output;

pub use logs::{console, errors};
pub use net::net;
pub use panel::open_devtools;
pub use perf::{State, on_event, perf};

/// Characters a listing prints before the rest of it goes to a file.
pub const BUDGET: usize = 32_000;

/// The last `last` of `items`, after how many earlier ones that leaves out.
pub fn tail<T>(items: &[T], last: usize) -> (usize, &[T]) {
    let earlier = items.len().saturating_sub(last);
    (earlier, &items[earlier..])
}

/// Adds the line saying a listing leaves out `earlier` older `noun`s, when it leaves any out.
pub fn note_earlier(text: &mut String, earlier: usize, noun: &str) {
    if earlier > 0 {
        let _ = writeln!(text, "… {} (raise -n)", output::count(earlier, noun));
    }
}

/// Runs `work` as a task of its own and waits for it. The daemon drops a command whose client
/// hangs up; the task still runs to its end, so what it switches on or off is not left half done.
pub async fn to_completion<T: Send + 'static>(work: impl Future<Output = Result<T>> + Send + 'static) -> Result<T> {
    tokio::spawn(work).await?
}

/// Refuses a file given to a recording's `start`: it goes with `stop` (e.g. `perf trace stop`).
fn file_goes_with_stop(stop: &str, path: Option<&Path>) -> Result<()> {
    match path {
        Some(path) => bail!("the file goes with stop: `hba {stop} {}`", path.display()),
        None => Ok(()),
    }
}

/// `path` against the caller's directory, or a new file in the runtime directory.
fn output_path(cx: &Cx<'_>, path: Option<&Path>, stem: &str, extension: &str) -> Result<PathBuf> {
    match path {
        Some(path) => Ok(cx.path(path)),
        None => cx.artifact(stem, extension),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listings_keep_the_latest_and_count_the_rest() {
        let items = [1, 2, 3, 4, 5];
        assert_eq!(tail(&items, 2), (3, &items[3..]));
        assert_eq!(tail(&items, 9), (0, &items[..]));
        let mut text = String::new();
        note_earlier(&mut text, 0, "earlier message");
        assert_eq!(text, "", "nothing left out, nothing said");
        note_earlier(&mut text, 1, "earlier message");
        note_earlier(&mut text, 3, "earlier request");
        assert_eq!(text, "… 1 earlier message (raise -n)\n… 3 earlier requests (raise -n)\n");
    }

    #[test]
    fn files_go_with_stop() {
        assert!(file_goes_with_stop("perf trace stop", None).is_ok());
        let error = file_goes_with_stop("net har stop", Some(Path::new("a.har"))).expect_err("start takes no file");
        assert_eq!(error.to_string(), "the file goes with stop: `hba net har stop a.har`");
    }
}
