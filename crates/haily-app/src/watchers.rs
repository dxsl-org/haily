//! Work-item watcher and proactive daemon startup — spawned identically for every
//! mode (this phase's fix for F6: GUI previously lacked the watcher, CLI lacked the
//! daemon; both are now unconditional, gated only by `BootstrapOptions`).
use haily_db::{queries::work_items, DbHandle};
use haily_io::{AdapterManager, Notification, WorkItemStatus};
use haily_proactive::ProactiveDaemon;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::info;

/// Poll active work items every second; broadcast changes to all adapters.
///
/// Adapters cache the snapshot and render it at their next natural update point
/// (e.g., before the `You:` prompt in the CLI), avoiding mid-output interleaving.
pub fn spawn_work_item_watcher(
    db: Arc<DbHandle>,
    am: AdapterManager,
    shutdown: CancellationToken,
    tasks: TaskTracker,
) {
    tasks.spawn(async move {
        let mut last_ids: Vec<String> = Vec::new();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("work item watcher shutting down");
                    break;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
            }

            let items = match work_items::list_active(&db).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("work item watcher: {e:#}");
                    continue;
                }
            };
            let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
            if ids == last_ids {
                continue;
            }
            last_ids = ids;
            let summaries: Vec<WorkItemStatus> = items
                .into_iter()
                .map(|i| WorkItemStatus {
                    title: i.title,
                    status: i.status,
                    progress: i.progress.min(100) as u8,
                    phase: i.phase,
                })
                .collect();
            am.notify_all(Notification::WorkItemsChanged(summaries)).await.ok();
        }
    });
}

/// Start the proactive daemon (morning brief, reminders, cross-domain alerts).
pub fn spawn_proactive_daemon(
    db: Arc<DbHandle>,
    am: AdapterManager,
    shutdown: CancellationToken,
    tasks: TaskTracker,
) {
    ProactiveDaemon::new(db, am).start(shutdown, &tasks);
}
