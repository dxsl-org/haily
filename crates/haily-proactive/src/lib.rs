mod cross_domain;
mod daily_rollup;
mod dnd;
mod morning_brief;
mod reminders;

use haily_db::DbHandle;
use haily_io::AdapterManager;
use std::future::Future;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::error;

/// Spawn a `'static` future onto `tasks`, logging if it ever exits — via panic,
/// unexpected early return, or (now) a normal shutdown-triggered `break` — instead of
/// dying silently. Registering on `tasks` (rather than bare `tokio::spawn`) means
/// `AppHandle::shutdown`'s `TaskTracker::wait()` blocks until this loop has actually
/// exited, not just been asked to.
fn spawn_logged<F>(name: &'static str, fut: F, tasks: &TaskTracker)
where
    F: Future<Output = ()> + Send + 'static,
{
    let handle = tasks.spawn(fut);
    tasks.spawn(async move {
        match handle.await {
            Ok(()) => error!(task = name, "proactive task exited (expected to run until shutdown)"),
            Err(join_err) if join_err.is_panic() => {
                error!(task = name, "proactive task panicked and died")
            }
            Err(join_err) => error!(task = name, error = %join_err, "proactive task was cancelled"),
        }
    });
}

/// Proactive background engine — fires morning briefs, reminders, cross-domain
/// alerts, and (Harness Completion phase 5) daily telemetry rollup + retention.
///
/// `start()` spawns four independent tokio tasks and returns immediately. Each loop
/// runs until `shutdown` is cancelled.
pub struct ProactiveDaemon {
    db: Arc<DbHandle>,
    am: AdapterManager,
}

impl ProactiveDaemon {
    pub fn new(db: Arc<DbHandle>, am: AdapterManager) -> Self {
        Self { db, am }
    }

    /// Spawn all background loops onto `tasks`, each holding a `child_token()` of
    /// `shutdown`. Non-blocking — returns immediately once tasks are registered.
    pub fn start(self, shutdown: CancellationToken, tasks: &TaskTracker) {
        let db = self.db;
        let am = self.am;

        spawn_logged(
            "morning_brief",
            morning_brief::loop_forever(db.clone(), am.clone(), shutdown.child_token()),
            tasks,
        );
        spawn_logged(
            "reminders",
            reminders::poll_loop(db.clone(), am.clone(), shutdown.child_token()),
            tasks,
        );
        spawn_logged(
            "cross_domain",
            cross_domain::alert_loop(db.clone(), am, shutdown.child_token()),
            tasks,
        );
        spawn_logged(
            "daily_rollup",
            daily_rollup::loop_forever(db, shutdown.child_token()),
            tasks,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panicking task must not crash the process or the watcher — the watcher's
    /// `handle.await` returns `Err(JoinError::is_panic() == true)` instead.
    #[tokio::test]
    async fn spawn_logged_survives_a_panicking_task() {
        let tasks = TaskTracker::new();
        spawn_logged(
            "test-panicker",
            async {
                panic!("simulated proactive loop crash");
            },
            &tasks,
        );
        // Give the spawned task + its watcher a chance to run; a successful return
        // here (no process abort, no propagated panic) is the assertion.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn spawn_logged_survives_early_return() {
        let tasks = TaskTracker::new();
        spawn_logged("test-early-return", async {}, &tasks);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    /// Proves the shutdown wiring end-to-end: cancelling `shutdown` makes all four
    /// loops (which otherwise sleep for up to 24h) exit promptly, and `TaskTracker`
    /// observes them as finished.
    #[tokio::test]
    async fn daemon_start_exits_all_loops_on_cancel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("haily.db");
        let db = Arc::new(DbHandle::init(&db_path).await.expect("db init"));
        let am = AdapterManager::builder().build();

        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();

        ProactiveDaemon::new(db, am).start(shutdown.clone(), &tasks);

        shutdown.cancel();
        tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(5), tasks.wait())
            .await
            .expect("all proactive loops must exit promptly on cancellation");
    }
}
