//! Work-item watcher and proactive daemon startup — spawned identically for every
//! mode (this phase's fix for F6: GUI previously lacked the watcher, CLI lacked the
//! daemon; both are now unconditional, gated only by `BootstrapOptions`).
use haily_db::{queries::journal, queries::work_items, DbHandle};
use haily_io::{AdapterManager, Notification, WorkItemStatus};
use haily_kms::KmsHandle;
use haily_proactive::ProactiveDaemon;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::info;

/// Convert a DB row to its wire-facing snapshot. Shared by `list_work_items_status`
/// and the poll loop below so both clamp `progress` identically.
fn to_status(item: work_items::WorkItem) -> WorkItemStatus {
    WorkItemStatus {
        title: item.title,
        status: item.status,
        progress: item.progress.min(100) as u8,
        phase: item.phase,
    }
}

/// Fetch the current active work-item set as the wire-facing `WorkItemStatus`
/// snapshot — used by the `list_work_items` Tauri command (on-mount reconcile path,
/// GUI phase 5). The poll loop below independently calls `work_items::list_active`
/// (it needs the raw rows' ids for its own diffing) and reuses `to_status`.
///
/// # Errors
/// Returns an error if the underlying query fails.
pub async fn list_work_items_status(db: &DbHandle) -> anyhow::Result<Vec<WorkItemStatus>> {
    let items = work_items::list_active(db).await?;
    Ok(items.into_iter().map(to_status).collect())
}

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
            let summaries: Vec<WorkItemStatus> = items.into_iter().map(to_status).collect();
            am.notify_all(Notification::WorkItemsChanged(summaries))
                .await
                .ok();
        }
    });
}

/// Start the proactive daemon (morning brief, reminders, cross-domain alerts).
///
/// `kms` is threaded in for the morning brief's synthesis (Phase 3, assistant-depth):
/// it correlates tasks/calendar/reminders and pulls floored memory context via
/// `KmsHandle::search_hybrid` — the same recall path `haily-core` uses for turn
/// context, so the daemon's recall behaves identically (no separate threshold logic).
pub fn spawn_proactive_daemon(
    db: Arc<DbHandle>,
    kms: Arc<KmsHandle>,
    am: AdapterManager,
    shutdown: CancellationToken,
    tasks: TaskTracker,
) {
    ProactiveDaemon::new(db, kms, am).start(shutdown, &tasks);
}

/// Interval in seconds between action-journal retention purges. Hourly is fine — the
/// retention window is measured in days, so a coarser cadence still bounds PII promptly.
const JOURNAL_PURGE_INTERVAL_SECS: u64 = 3600;

/// Periodically purge action-journal rows past their `retention_expires_at` (phase 3,
/// C-security). Bounds recorded PII. Selects on shutdown and is tracked, so a purge in
/// progress finishes (or the sleep is interrupted) rather than being abandoned silently.
pub fn spawn_journal_purge(db: Arc<DbHandle>, shutdown: CancellationToken, tasks: TaskTracker) {
    tasks.spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("journal purge task shutting down");
                    break;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(JOURNAL_PURGE_INTERVAL_SECS)) => {}
            }
            match journal::purge_expired(&db).await {
                Ok(n) if n > 0 => info!(count = n, "purged expired action-journal rows"),
                Err(e) => tracing::warn!("journal purge failed: {e:#}"),
                _ => {}
            }
        }
    });
}

/// Spawn the scheduled GFS (grandfather-father-son) SQLite backup worker (Phase 6,
/// "Activate & Measure" — full design in `haily_proactive::backup`). Registered
/// separately from `ProactiveDaemon` (mirrors `spawn_journal_purge`, not one of the
/// daemon's fixed four loops) since it needs extra construction-time arguments the
/// daemon's other loops don't (a filesystem directory, a credential-posture bool).
///
/// `credential_migration_clean` is a boot-time snapshot computed in `bootstrap.rs` —
/// the only layer with visibility into `CredentialStore`/keyring state, since this
/// crate's `haily-proactive` dependency sits BELOW `haily-app` and must not reach back
/// up into it. Passed down as a plain `bool` (plus the preference key names that may
/// hold a plaintext credential, M7b) so the worker itself stays ignorant of keyring
/// internals entirely — it only knows "scrub these preference keys from the copy if
/// migration wasn't clean," never why.
pub fn spawn_backup(
    db: Arc<DbHandle>,
    backups_dir: PathBuf,
    credential_migration_clean: bool,
    credential_preference_keys: Vec<String>,
    shutdown: CancellationToken,
    tasks: TaskTracker,
) {
    tasks.spawn(haily_proactive::backup::loop_forever(
        db,
        backups_dir,
        credential_migration_clean,
        credential_preference_keys,
        shutdown,
    ));
}
