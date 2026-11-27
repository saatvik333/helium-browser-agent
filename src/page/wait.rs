//! Navigating and waiting: one deadline through every phase of a wait, a settled page after
//! actions, and the report of what an action changed.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;

use super::{Mark, Page, Signal};
use crate::cli::LoadState;
use crate::undo::Undo;

/// The DOM counts as settled after this long without mutations or new requests.
const QUIET: Duration = Duration::from_millis(100);

/// Longest an action waits for the page to settle when nothing navigates.
const SETTLE_CAP: Duration = Duration::from_millis(1500);

/// How long `--wait dom` gives the load event after DOMContentLoaded, so scripts can render
/// without the wait hanging on slow third parties.
const RENDER_WAIT: Duration = Duration::from_millis(1500);

/// How long `--wait dom` lets the DOM settle after that.
const DOM_QUIET: Duration = Duration::from_secs(1);

/// When a command's wait gives up: one instant for all its phases, so `--timeout` bounds the
/// whole wait, and the timeout it came from, for the message.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    at: tokio::time::Instant,
    timeout: Duration,
}

impl Deadline {
    pub fn after(timeout: Duration) -> Self {
        Self { at: tokio::time::Instant::now() + timeout, timeout }
    }

    /// Time left, zero once the deadline has passed.
    pub fn left(&self) -> Duration {
        self.at.saturating_duration_since(tokio::time::Instant::now())
    }

    /// The error of a wait for `what` that reached the deadline.
    fn missed(&self, what: &str) -> anyhow::Error {
        anyhow!("timed out after {:.1}s waiting for {what}", self.timeout.as_secs_f32())
    }
}

impl Page {
    /// Navigates the main frame and waits for `wait`, all within `timeout`.
    pub async fn goto(&self, url: &str, wait: LoadState, timeout: Duration) -> Result<()> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Navigated {
            #[serde(default)]
            loader_id: Option<String>,
            #[serde(default)]
            error_text: Option<String>,
            /// The response was a download (an attachment), not a document.
            #[serde(default)]
            is_download: bool,
        }
        let deadline = Deadline::after(timeout);
        let session = self.session()?;
        let mut signals = self.subscribe();
        // The browser answers once the server has; a slow one can take any time.
        let navigate = self.heading_to(url, self.renderer_call("Page.navigate", json!({"url": url}), &session));
        let navigated: Navigated = tokio::time::timeout_at(deadline.at, navigate)
            .await
            .map_err(|_| deadline.missed(&format!("{url} to answer")))??;
        if navigated.is_download {
            return Ok(());
        }
        if let Some(error) = navigated.error_text.filter(|e| !e.is_empty()) {
            bail!("navigation to {url} failed: {error}");
        }
        let Some(loader) = navigated.loader_id else {
            return Ok(()); // Same-document navigation.
        };
        let what = format!("the navigation to {url} to commit");
        self.wait_for(&mut signals, deadline, &what, || self.loader().as_deref() == Some(loader.as_str())).await?;
        self.wait_load(wait, deadline).await
    }

    /// Starts a main-frame navigation to `url` with the protocol call `start`. Console capture
    /// follows the destination first, before its document can run any script. When nothing
    /// navigates (the call fails, or the command is cancelled before it), the navigation is no
    /// longer pending and capture follows the current document again.
    pub async fn heading_to<T>(&self, url: &str, start: impl Future<Output = Result<T>>) -> Result<T> {
        let starts = {
            let mut state = self.state();
            state.pending = Some(url.to_owned());
            state.starts
        };
        let stay = Undo::new({
            let (page, url) = (self.arc(), url.to_owned());
            async move { page.stayed(&url, starts) }
        });
        let started = async {
            crate::console::apply(self).await?;
            start.await
        }
        .await;
        if started.is_ok() {
            stay.disarm();
        } else {
            stay.finish().await;
        }
        started
    }

    /// Forgets the pending navigation to `url` that [`Page::heading_to`] set when `starts`
    /// navigations had started, unless one started since.
    fn stayed(&self, url: &str, starts: u64) {
        let mut state = self.state();
        if state.starts != starts || state.pending.as_deref() != Some(url) {
            return;
        }
        state.pending = None;
        drop(state);
        crate::console::reconsider(self);
    }

    /// Waits for a load state of the current document.
    pub async fn wait_load(&self, wait: LoadState, deadline: Deadline) -> Result<()> {
        const LOAD: &str = "the load event (a slow resource holds it up; wait for `dom` instead)";
        let mut signals = self.subscribe();
        let fired = |name: &str| self.state().fired(name);
        match wait {
            LoadState::None | LoadState::Commit => Ok(()),
            LoadState::Load => self.wait_for(&mut signals, deadline, LOAD, || fired("load")).await,
            LoadState::Idle => {
                self.wait_for(&mut signals, deadline, LOAD, || fired("load")).await?;
                let what = "the networkIdle event (the page keeps the network busy; wait for `load` instead)";
                self.wait_for(&mut signals, deadline, what, || fired("networkIdle")).await
            }
            LoadState::Dom => {
                self.wait_for(&mut signals, deadline, "the DOMContentLoaded event", || fired("DOMContentLoaded"))
                    .await?;
                let render = Deadline::after(RENDER_WAIT.min(deadline.left()));
                let _ = self.wait_for(&mut signals, render, "the load event", || fired("load")).await;
                self.quiet(Instant::now(), DOM_QUIET.min(deadline.left())).await;
                Ok(())
            }
        }
    }

    /// Polls `done` on every page signal until it holds, failing at `deadline` with an error
    /// that names `what` was awaited.
    pub async fn wait_for(
        &self,
        signals: &mut broadcast::Receiver<Signal>,
        deadline: Deadline,
        what: &str,
        mut done: impl FnMut() -> bool,
    ) -> Result<()> {
        loop {
            if done() {
                return Ok(());
            }
            if let Some(dialog) = self.dialog() {
                return Err(Self::dialog_error(&dialog));
            }
            match tokio::time::timeout_at(deadline.at, signals.recv()).await {
                Err(_) => return Err(deadline.missed(what)),
                Ok(Ok(Signal::Detached) | Err(broadcast::error::RecvError::Closed)) => bail!("the tab was closed"),
                Ok(_) => {}
            }
        }
    }

    /// [`Page::wait_for`] a page change, for at most `timeout`.
    pub async fn wait_until(
        &self,
        signals: &mut broadcast::Receiver<Signal>,
        timeout: Duration,
        done: impl FnMut() -> bool,
    ) -> Result<()> {
        self.wait_for(signals, Deadline::after(timeout), "the page", done).await
    }

    /// Waits until the DOM has not changed for [`QUIET`] and no document, fetch or XHR started
    /// after `since` is in flight, for at most `cap`. Earlier requests (long polls, beacons) are
    /// ignored. Returns early when a navigation or dialog starts.
    pub async fn quiet(&self, since: Instant, cap: Duration) {
        let deadline = Instant::now() + cap;
        let starts = self.state().starts;
        let mut last = None;
        let mut stable_from = Instant::now();
        while Instant::now() < deadline {
            let count = match self.main_world().await {
                Ok(world) => self.call(&world, "function() { return this.hbaMutations; }", &[]).await.ok(),
                Err(_) => None,
            };
            if self.dialog().is_some() || self.state().starts != starts {
                return;
            }
            let busy = self.network.lock().expect("network lock").busy_since(since);
            crate::trace!("quiet: mutations {count:?} busy {busy} stable {:?}", stable_from.elapsed());
            if count != last || busy {
                last = count;
                stable_from = Instant::now();
            } else if stable_from.elapsed() >= QUIET {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub fn mark(&self) -> Mark {
        let state = self.state();
        Mark {
            navigations: state.navigations,
            starts: state.starts,
            url: state.url.clone(),
            console_seq: self.console.lock().expect("console lock").seq(),
            network_seq: self.network.lock().expect("network lock").seq(),
            download_seq: self.downloads.lock().expect("downloads lock").seq(),
            at: Instant::now(),
        }
    }

    /// After an input action: waits, within `timeout`, for the navigation it started, or for
    /// the DOM to settle.
    pub async fn settle(&self, mark: &Mark, timeout: Duration) -> Result<()> {
        let deadline = Deadline::after(timeout);
        self.quiet(mark.at, SETTLE_CAP.min(deadline.left())).await;
        let navigating = {
            let state = self.state();
            state.starts != mark.starts || state.navigations != mark.navigations
        };
        if !navigating || self.dialog().is_some() || !self.is_live() {
            return Ok(());
        }
        // A started navigation may still be waiting for the network before it commits, or end
        // without committing: a download or a 204 response stops the frame loading instead.
        let downloaded = || {
            let downloads = self.downloads.lock().expect("downloads lock");
            downloads.items.iter().next_back().is_some_and(|item| item.started >= mark.at)
        };
        let ended = || {
            let state = self.state();
            state.navigations != mark.navigations || state.pending.is_none()
        };
        let mut signals = self.subscribe();
        // A dialog ends the wait too (as an error); the report names it.
        let what = "the navigation to end";
        let waited = self.wait_for(&mut signals, deadline, what, || ended() || downloaded()).await;
        if waited.is_err() || self.dialog().is_some() {
            return Ok(());
        }
        if self.navigations() != mark.navigations {
            self.wait_load(LoadState::Dom, deadline).await.ok();
        } else if !downloaded() && self.network.lock().expect("network lock").turned_into_download(mark.at) {
            // The browser announces the download just after the navigation ends; report it.
            let announced = Deadline::after(deadline.left().min(Duration::from_secs(1)));
            self.wait_for(&mut signals, announced, "the download", downloaded).await.ok();
        }
        Ok(())
    }

    /// Appends what changed since `mark` (navigation, dialog, errors, downloads).
    pub fn report(&self, mark: &Mark, out: &mut String) {
        use std::fmt::Write as _;
        let (url, navigations) = {
            let state = self.state();
            (state.url.clone(), state.navigations)
        };
        if url != mark.url {
            let _ = writeln!(out, "→ {}", crate::output::short_url(&url, 120));
        } else if navigations != mark.navigations {
            let _ = writeln!(out, "↻ reloaded");
        }
        for note in self.take_notes() {
            let _ = writeln!(out, "· {note}");
        }
        if let Some(dialog) = self.dialog() {
            let _ = writeln!(out, "! {} dialog: {:?} — `hba dialog accept|dismiss`", dialog.kind, dialog.message);
        }
        self.console.lock().expect("console lock").report_since(mark.console_seq, out);
        self.network.lock().expect("network lock").report_since(mark.network_seq, out);
        self.downloads.lock().expect("downloads lock").report_since(mark.download_seq, out);
    }
}
