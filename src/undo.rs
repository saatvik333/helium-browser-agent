//! Cleanup that survives cancellation. The daemon drops a command's future when its client hangs
//! up, which skips every statement after the `.await` it was parked on, so state a command switches
//! on (interception, `Runtime`, helper tabs) is switched off by a guard instead.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

type Task = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Runs its undo when dropped, as a task on the current runtime, unless [`Undo::finish`] or
/// [`Undo::disarm`] consumed it first.
#[must_use = "dropping the guard runs the undo at once"]
pub struct Undo(
    // Only ever reached through `&mut self`; the mutex makes the guard `Sync`, so commands that
    // hold one across an await stay `Send`.
    Mutex<Option<Task>>,
);

impl Undo {
    /// A guard that runs `undo` unless it is finished or disarmed first.
    pub fn new(undo: impl Future<Output = ()> + Send + 'static) -> Self {
        Self(Mutex::new(Some(Box::pin(undo))))
    }

    /// Runs the undo now and waits for it. The undo is a task of its own, so a command cancelled
    /// while waiting does not cut it short.
    pub async fn finish(mut self) {
        if let Some(undo) = self.take() {
            let _ = tokio::spawn(undo).await;
        }
    }

    /// Drops the undo without running it: what it would restore was handed on.
    pub fn disarm(mut self) {
        self.take();
    }

    fn take(&mut self) -> Option<Task> {
        self.0.get_mut().expect("undo lock").take()
    }
}

impl Drop for Undo {
    fn drop(&mut self) {
        if let Some(undo) = self.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(undo);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Undo;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn counting(runs: &Arc<AtomicU32>) -> Undo {
        let runs = Arc::clone(runs);
        Undo::new(async move {
            runs.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[tokio::test]
    async fn a_cancelled_command_still_runs_its_undo_once() {
        let runs = Arc::new(AtomicU32::new(0));
        let guard = counting(&runs);
        let command = async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        };
        // Dropping the parked future is what the daemon does when the client hangs up.
        let cancelled = tokio::time::timeout(std::time::Duration::from_millis(10), command).await;
        assert!(cancelled.is_err());
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        counting(&runs).finish().await;
        assert_eq!(runs.load(Ordering::SeqCst), 2);

        counting(&runs).disarm();
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }
}
