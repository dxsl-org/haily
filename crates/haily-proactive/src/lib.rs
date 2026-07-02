mod cross_domain;
mod dnd;
mod morning_brief;
mod reminders;

use haily_db::DbHandle;
use haily_io::AdapterManager;
use std::future::Future;
use std::sync::Arc;
use tracing::error;

/// Spawn a `'static` future as a detached task, logging if it ever exits — via panic
/// or unexpected early return — instead of dying silently.
///
/// The three proactive loops are meant to run forever (`loop { .. }` with no break).
/// A bug that causes one to panic currently drops the `JoinHandle` with no signal, so
/// the daemon looks alive while one of its capabilities has quietly stopped. This
/// spawns a watcher task that awaits the loop's `JoinHandle` and logs on any exit.
fn spawn_logged<F>(name: &'static str, fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let handle = tokio::spawn(fut);
    tokio::spawn(async move {
        match handle.await {
            Ok(()) => error!(task = name, "proactive task exited (expected to run forever)"),
            Err(join_err) if join_err.is_panic() => {
                error!(task = name, "proactive task panicked and died")
            }
            Err(join_err) => error!(task = name, error = %join_err, "proactive task was cancelled"),
        }
    });
}

/// Proactive background engine — fires morning briefs, reminders, and cross-domain alerts.
///
/// `start()` spawns three independent tokio tasks and returns immediately.
pub struct ProactiveDaemon {
    db: Arc<DbHandle>,
    am: AdapterManager,
}

impl ProactiveDaemon {
    pub fn new(db: Arc<DbHandle>, am: AdapterManager) -> Self {
        Self { db, am }
    }

    /// Spawn all background loops. Non-blocking — each loop runs as a detached task.
    pub fn start(self) {
        let db = self.db;
        let am = self.am;

        spawn_logged("morning_brief", morning_brief::loop_forever(db.clone(), am.clone()));
        spawn_logged("reminders", reminders::poll_loop(db.clone(), am.clone()));
        spawn_logged("cross_domain", cross_domain::alert_loop(db, am));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panicking task must not crash the process or the watcher — the watcher's
    /// `handle.await` returns `Err(JoinError::is_panic() == true)` instead.
    #[tokio::test]
    async fn spawn_logged_survives_a_panicking_task() {
        spawn_logged("test-panicker", async {
            panic!("simulated proactive loop crash");
        });
        // Give the spawned task + its watcher a chance to run; a successful return
        // here (no process abort, no propagated panic) is the assertion.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn spawn_logged_survives_early_return() {
        spawn_logged("test-early-return", async {});
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
