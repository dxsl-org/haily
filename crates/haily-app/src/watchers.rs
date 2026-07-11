//! Work-item watcher and proactive daemon startup — spawned identically for every
//! mode (this phase's fix for F6: GUI previously lacked the watcher, CLI lacked the
//! daemon; both are now unconditional, gated only by `BootstrapOptions`).
use haily_db::{queries::journal, queries::work_items, DbHandle};
use haily_io::{AdapterManager, Notification, RunEvent, WorkItemStatus};
use haily_kms::KmsHandle;
use haily_proactive::ProactiveDaemon;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::info;
use uuid::Uuid;

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

/// Drain a pipeline run's ordered `RunEvent` stream into the adapter that owns `session_id`
/// (Sub-Agent + Skill Architecture phase 11a).
///
/// This is the app-layer BRIDGE that preserves the "core never imports io" invariant: the
/// P4 runner (in `haily-core`) emits `RunEvent`s to a plain `mpsc` it is handed, knowing
/// nothing about adapters; this loop — living above both `haily-core` and `haily-io` — is
/// the only place the two meet, forwarding each event to `AdapterManager::deliver_run_event`
/// (the sanitize + ordered-delivery chokepoint). Mirrors how the phase-08
/// distillation→notify bridge was left an app-layer concern for the same reason.
///
/// Ordering + backpressure are preserved end-to-end: `events` is a bounded FIFO from the
/// runner, and `deliver_run_event` awaits a full per-adapter channel rather than dropping —
/// so a fast build log slows the runner instead of losing events. The loop ends when the
/// runner drops its sender (run finished) or on shutdown, whichever comes first; it is
/// registered on `tasks` so shutdown drains it.
pub fn spawn_run_event_bridge(
    session_id: Uuid,
    mut events: mpsc::Receiver<RunEvent>,
    am: AdapterManager,
    shutdown: CancellationToken,
    tasks: TaskTracker,
) {
    tasks.spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!(%session_id, "run-event bridge shutting down");
                    break;
                }
                maybe = events.recv() => {
                    let Some(event) = maybe else {
                        // Runner dropped its sender — the run is over.
                        break;
                    };
                    if let Err(e) = am.deliver_run_event(session_id, event).await {
                        // A closed adapter channel or an unbound session is not fatal to the
                        // run — log and keep draining so the runner is never blocked by a
                        // dead consumer.
                        tracing::warn!(%session_id, "run-event delivery failed: {e:#}");
                    }
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use haily_io::{Adapter, RequestSender, ResponseChunk};

    /// Minimal adapter that echoes every delivered `RunEvent` onto a test-visible mpsc, so
    /// the bridge's drain→deliver forwarding + ordering can be asserted through a real
    /// `AdapterManager` route.
    struct RecordingAdapter {
        tx: mpsc::Sender<RunEvent>,
    }

    #[async_trait]
    impl Adapter for RecordingAdapter {
        async fn start(&self, _tx: RequestSender) -> Result<()> {
            Ok(())
        }
        async fn deliver(&self, _session_id: Uuid, _chunk: ResponseChunk) -> Result<()> {
            Ok(())
        }
        async fn deliver_run_event(&self, _session_id: Uuid, event: RunEvent) -> Result<()> {
            let _ = self.tx.send(event).await;
            Ok(())
        }
        async fn notify(&self, _msg: Notification) -> Result<()> {
            Ok(())
        }
        fn id(&self) -> &str {
            "rec"
        }
    }

    /// The bridge forwards the runner's ordered event stream to the owning adapter in full,
    /// in order, and exits cleanly when the runner drops its sender.
    #[tokio::test]
    async fn bridge_forwards_run_events_in_order_and_exits_on_sender_drop() {
        let (seen_tx, mut seen_rx) = mpsc::channel::<RunEvent>(16);
        let adapter = Arc::new(RecordingAdapter { tx: seen_tx });
        let am = AdapterManager::builder().register(adapter).build();
        let session = Uuid::new_v4();
        am.bind_session(session, "rec");

        let (ev_tx, ev_rx) = mpsc::channel::<RunEvent>(16);
        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();
        spawn_run_event_bridge(session, ev_rx, am, shutdown, tasks.clone());

        for seq in 0..3u64 {
            ev_tx
                .send(RunEvent::StageOutput { run_id: "r".into(), seq, chunk: format!("l{seq}") })
                .await
                .unwrap();
        }
        drop(ev_tx); // runner finished → bridge loop should end

        let mut seen = Vec::new();
        for _ in 0..3 {
            match seen_rx.recv().await.expect("event forwarded") {
                RunEvent::StageOutput { seq, .. } => seen.push(seq),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(seen, vec![0, 1, 2], "bridge must preserve order and drop none");

        tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(2), tasks.wait())
            .await
            .expect("bridge task must exit after the runner drops its sender");
    }
}
