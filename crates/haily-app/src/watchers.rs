//! Work-item watcher and proactive daemon startup — spawned identically for every
//! mode (this phase's fix for F6: GUI previously lacked the watcher, CLI lacked the
//! daemon; both are now unconditional, gated only by `BootstrapOptions`).
use haily_db::{queries::journal, queries::run_events, queries::work_items, DbHandle};
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
/// (Sub-Agent + Skill Architecture phase 11a), then persist it (Unified Chat UI phase 5, D2).
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
///
/// Persistence is DELIVER-FIRST, PERSIST-AFTER and best-effort: a DB write never gates or
/// delays the live delivery above, and a write failure is logged, never propagated — a run
/// must never stall because its history couldn't be saved. One task instance runs PER RUN
/// (this function is called fresh per launch, at three call sites), each writing only its own
/// `run_id`, so concurrent runs never contend on each other's rows. `StageOutput` carries no
/// `stage` field of its own (`haily_types::RunEvent`), so `current_stage` tracks the most
/// recently seen `StageStarted` for THIS run and keys the marker upsert with it.
pub fn spawn_run_event_bridge(
    session_id: Uuid,
    mut events: mpsc::Receiver<RunEvent>,
    am: AdapterManager,
    db: Arc<DbHandle>,
    shutdown: CancellationToken,
    tasks: TaskTracker,
) {
    tasks.spawn(async move {
        let mut current_stage = String::new();
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

                    if let RunEvent::StageStarted { ref stage, .. } = event {
                        current_stage = stage.clone();
                    }
                    // Captured BEFORE delivery moves `event`: StageOutput needs only `run_id`/
                    // `seq` (its `chunk` text is never cloned just for marker bookkeeping);
                    // every other variant is small, so cloning it whole for its persisted row
                    // is cheap and keeps this one match the single source of truth for
                    // "row vs. marker."
                    let row_to_persist = match &event {
                        RunEvent::StageOutput { .. } => None,
                        other => Some(other.clone()),
                    };
                    let marker_to_persist = match &event {
                        RunEvent::StageOutput { run_id, seq, .. } => Some((run_id.clone(), *seq)),
                        _ => None,
                    };

                    if let Err(e) = am.deliver_run_event(session_id, event).await {
                        // A closed adapter channel or an unbound session is not fatal to the
                        // run — log and keep draining so the runner is never blocked by a
                        // dead consumer.
                        tracing::warn!(%session_id, "run-event delivery failed: {e:#}");
                    }

                    if let Some(ev) = row_to_persist {
                        let run_id = run_events::run_id_of(&ev).to_string();
                        if let Err(e) = run_events::insert_run_event(&db, &run_id, &ev).await {
                            tracing::warn!(%session_id, "run-event persist failed: {e:#}");
                        }
                    } else if let Some((run_id, seq)) = marker_to_persist {
                        if let Err(e) =
                            run_events::upsert_stage_marker(&db, &run_id, &current_stage, seq).await
                        {
                            tracing::warn!(%session_id, "run-event marker persist failed: {e:#}");
                        }
                    }
                }
            }
        }
    });
}

/// Drain a run's distillation-proposal stream into every adapter (Sub-Agent + Skill Architecture
/// phase 8's DEP-C2 emit seam, closed live by the "Pipeline Activation & Wiring" plan, phase 1 —
/// Seam 3). Mirrors `spawn_run_event_bridge`'s shape exactly: the P6 build pipeline (in
/// `haily-core`) emits a `Notification::DistillationProposal` to a plain `mpsc` it is handed,
/// knowing nothing about adapters; this loop is the only place core and io meet, broadcasting
/// each proposal via `AdapterManager::notify_all` (the GUI cockpit renders it as a
/// `ProactiveCardKind::DistillationProposal` card). Ends when the run drops its sender or on
/// shutdown, whichever comes first; registered on `tasks` so shutdown drains it.
pub fn spawn_distillation_bridge(
    mut proposals: mpsc::Receiver<Notification>,
    am: AdapterManager,
    shutdown: CancellationToken,
    tasks: TaskTracker,
) {
    tasks.spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("distillation bridge shutting down");
                    break;
                }
                maybe = proposals.recv() => {
                    let Some(notification) = maybe else {
                        // Run finished (or never emitted) — its sender dropped.
                        break;
                    };
                    if let Err(e) = am.notify_all(notification).await {
                        tracing::warn!("distillation notify failed: {e:#}");
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
        notify_tx: mpsc::Sender<Notification>,
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
        async fn notify(&self, msg: Notification) -> Result<()> {
            let _ = self.notify_tx.send(msg).await;
            Ok(())
        }
        fn id(&self) -> &str {
            "rec"
        }
    }

    /// Fresh in-memory-backed `DbHandle` for a bridge test — mirrors `haily-db`'s own
    /// `tempfile::tempdir()` test idiom; the `TempDir` guard must outlive every DB call or the
    /// directory is removed before queries run (see feedback-tempdir-guard-dropped-in-test-helper).
    async fn test_db() -> (Arc<DbHandle>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        (Arc::new(db), dir)
    }

    /// The bridge forwards the runner's ordered event stream to the owning adapter in full,
    /// in order, and exits cleanly when the runner drops its sender.
    #[tokio::test]
    async fn bridge_forwards_run_events_in_order_and_exits_on_sender_drop() {
        let (seen_tx, mut seen_rx) = mpsc::channel::<RunEvent>(16);
        let (notify_tx, _notify_rx) = mpsc::channel::<Notification>(1);
        let adapter = Arc::new(RecordingAdapter {
            tx: seen_tx,
            notify_tx,
        });
        let am = AdapterManager::builder().register(adapter).build();
        let session = Uuid::new_v4();
        am.bind_session(session, "rec");
        let (db, _dir) = test_db().await;

        let (ev_tx, ev_rx) = mpsc::channel::<RunEvent>(16);
        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();
        spawn_run_event_bridge(session, ev_rx, am, db, shutdown, tasks.clone());

        for seq in 0..3u64 {
            ev_tx
                .send(RunEvent::StageOutput {
                    run_id: "r".into(),
                    seq,
                    chunk: format!("l{seq}"),
                })
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
        assert_eq!(
            seen,
            vec![0, 1, 2],
            "bridge must preserve order and drop none"
        );

        tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(2), tasks.wait())
            .await
            .expect("bridge task must exit after the runner drops its sender");
    }

    /// The bridge persists non-`StageOutput` events as `run_events` rows and routes
    /// `StageOutput` chunks into a text-free `run_stage_marker` keyed by the last-seen
    /// `StageStarted` stage — never a row, never the chunk's own text.
    #[tokio::test]
    async fn bridge_persists_rows_and_stage_markers_after_delivery() {
        use haily_db::queries::{run_events, sessions};

        let (seen_tx, mut seen_rx) = mpsc::channel::<RunEvent>(16);
        let (notify_tx, _notify_rx) = mpsc::channel::<Notification>(1);
        let adapter = Arc::new(RecordingAdapter {
            tx: seen_tx,
            notify_tx,
        });
        let am = AdapterManager::builder().register(adapter).build();
        let session = Uuid::new_v4();
        am.bind_session(session, "rec");
        let (db, _dir) = test_db().await;
        let session_id_str = session.to_string();
        sessions::create_session(&db, &session_id_str, "test", None)
            .await
            .unwrap();

        let (ev_tx, ev_rx) = mpsc::channel::<RunEvent>(16);
        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();
        spawn_run_event_bridge(session, ev_rx, am, Arc::clone(&db), shutdown, tasks.clone());

        let run_id = "r1".to_string();
        ev_tx
            .send(RunEvent::RunStarted {
                run_id: run_id.clone(),
                work_item_id: "w1".into(),
            })
            .await
            .unwrap();
        ev_tx
            .send(RunEvent::StageStarted {
                run_id: run_id.clone(),
                stage: "build".into(),
                tier: None,
            })
            .await
            .unwrap();
        ev_tx
            .send(RunEvent::StageOutput {
                run_id: run_id.clone(),
                seq: 7,
                chunk: "SECRET=xyz".into(),
            })
            .await
            .unwrap();
        drop(ev_tx);

        for _ in 0..3 {
            seen_rx
                .recv()
                .await
                .expect("event forwarded before persistence");
        }

        tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(2), tasks.wait())
            .await
            .expect("bridge task must exit after the runner drops its sender");

        let persisted = run_events::list_run_events(&db, &run_id).await.unwrap();
        assert_eq!(
            persisted.len(),
            2,
            "RunStarted + StageStarted persist; StageOutput does not"
        );
        assert!(
            !persisted
                .iter()
                .any(|e| matches!(e, RunEvent::StageOutput { .. })),
            "StageOutput must never appear as a persisted row"
        );

        let markers = run_events::list_stage_markers(&db, &run_id).await.unwrap();
        assert_eq!(markers.len(), 1);
        assert_eq!(
            markers[0].stage, "build",
            "marker keyed by the last-seen StageStarted stage"
        );
        assert_eq!(markers[0].count, 1);
        assert_eq!(markers[0].last_seq, 7);
    }

    /// The distillation bridge forwards a proposal to every adapter via `notify_all` and exits
    /// cleanly when the run drops its sender (mirrors the `RunEvent` bridge's own contract).
    #[tokio::test]
    async fn distillation_bridge_forwards_proposals_and_exits_on_sender_drop() {
        let (seen_tx, _seen_rx) = mpsc::channel::<RunEvent>(1);
        let (notify_tx, mut notify_rx) = mpsc::channel::<Notification>(16);
        let adapter = Arc::new(RecordingAdapter {
            tx: seen_tx,
            notify_tx,
        });
        let am = AdapterManager::builder().register(adapter).build();

        let (dist_tx, dist_rx) = mpsc::channel::<Notification>(16);
        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();
        spawn_distillation_bridge(dist_rx, am, shutdown, tasks.clone());

        dist_tx
            .send(Notification::DistillationProposal {
                class_key: "compile_error:auth".to_string(),
                summary: "recurring pattern".to_string(),
                rule_count: 2,
            })
            .await
            .unwrap();
        drop(dist_tx); // run finished → bridge loop should end

        match notify_rx.recv().await.expect("proposal forwarded") {
            Notification::DistillationProposal {
                class_key,
                rule_count,
                ..
            } => {
                assert_eq!(class_key, "compile_error:auth");
                assert_eq!(rule_count, 2);
            }
            other => panic!("unexpected {other:?}"),
        }

        tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(2), tasks.wait())
            .await
            .expect("distillation bridge task must exit after the run drops its sender");
    }
}
