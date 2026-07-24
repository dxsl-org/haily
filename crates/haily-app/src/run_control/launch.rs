//! The ONE launch path `haily-app`'s `launch.rs` (GUI "New run") and `trigger.rs` (slash/chat-
//! intent) both call (Unified Chat UI phase 6, D3) — replaces the bridge-wiring + task-spawn
//! each used to duplicate. Registering the run's cancel/pause handles happens SYNCHRONOUSLY here,
//! before the tracked task is spawned, so a `kill_run` issued the instant after this function
//! returns still has a token to cancel.
use crate::notify::{OsNotifier, ToastCoalescer};
use crate::run_control::RunControlRegistry;
use haily_core::{CodingRunSpec, Orchestrator};
use haily_db::DbHandle;
use haily_io::AdapterManager;
use haily_types::{Notification, ResponseChunk};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Mirrors the eval harness's own `1024` — generous headroom above a typical run's event count
/// so a stage-output burst never blocks the runner mid-stage waiting on a slow adapter drain.
const RUN_EVENTS_CAPACITY: usize = 1024;
/// At most a handful of distillation proposals surface per run (Ship-stage-only emission).
const DISTILLATION_CAPACITY: usize = 16;

/// The handles [`spawn_launch`] needs, bundled to keep its arity sane (mirrors
/// `pipeline::LaunchDeps`/`trigger::LaunchHandles`'s own bundling convention). Built by the
/// caller from either `&AppHandle` (`launch.rs`) or the raw dispatch-loop handles (`trigger.rs`).
pub struct LaunchCtx {
    pub orc: Arc<Orchestrator>,
    pub am: AdapterManager,
    pub tasks: TaskTracker,
    pub db: Arc<DbHandle>,
    pub registry: Arc<RunControlRegistry>,
    /// Unified Chat UI phase 7 (D7) — threaded into `spawn_run_event_bridge` so this launch's
    /// `RunComplete`/`RunPaused`/`ApprovalNeeded` events can fire an OS toast.
    pub notifier: Arc<dyn OsNotifier>,
    pub coalescer: Arc<ToastCoalescer>,
}

/// Register `spec.run_id`'s control handles and spawn the tracked task driving it to completion.
/// `run_cancel` is the CALLER's token (mirrors the pre-existing `launch.rs`/`trigger.rs`
/// difference: a GUI-initiated launch gets a fresh child of the root shutdown token, a
/// chat-triggered launch reuses the SAME per-turn token its "Stop" action already cancels) — this
/// function never constructs it, only registers + forwards it.
///
/// Mirrors `Orchestrator::launch_coding_run`'s own terminal-`Complete` contract: on a setup
/// failure this sends `Error` + `Complete` on `resp_tx`; success sends `Complete` itself deeper
/// in the call chain. The caller's existing delivery-drain task needs no special-casing.
pub fn spawn_launch(
    ctx: LaunchCtx,
    spec: CodingRunSpec,
    run_cancel: CancellationToken,
    resp_tx: mpsc::Sender<ResponseChunk>,
) {
    let run_id = spec.run_id.to_string();
    let session_id = spec.session_id;
    let pause = Arc::new(AtomicBool::new(false));
    ctx.registry
        .register(&run_id, run_cancel.clone(), Arc::clone(&pause));

    let (events_tx, events_rx) = mpsc::channel(RUN_EVENTS_CAPACITY);
    let (dist_tx, dist_rx) = mpsc::channel::<Notification>(DISTILLATION_CAPACITY);

    crate::watchers::spawn_run_event_bridge(
        session_id,
        events_rx,
        ctx.am.clone(),
        Arc::clone(&ctx.db),
        Arc::clone(&ctx.registry),
        Arc::clone(&ctx.notifier),
        Arc::clone(&ctx.coalescer),
        run_cancel.clone(),
        ctx.tasks.clone(),
    );
    crate::watchers::spawn_distillation_bridge(
        dist_rx,
        ctx.am,
        run_cancel.clone(),
        ctx.tasks.clone(),
    );

    let orc = Arc::clone(&ctx.orc);
    let registry = Arc::clone(&ctx.registry);
    let db = Arc::clone(&ctx.db);
    let run_id_for_err = run_id.clone();
    ctx.tasks.spawn(async move {
        let result = orc
            .launch_coding_run(
                spec,
                pause,
                resp_tx.clone(),
                events_tx,
                Some(dist_tx),
                run_cancel,
            )
            .await;
        if let Err(e) = result {
            tracing::error!("coding run launch failed: {e:#}");
            reconcile_failed_launch(&db, &registry, &run_id_for_err).await;
            let _ = resp_tx
                .send(ResponseChunk::Error(format!("⚠️ {e:#}")))
                .await;
            let _ = resp_tx.send(ResponseChunk::Complete).await;
        }
    });
}

/// REVIEW FIX (MED, phase-06 review): a setup failure inside `launch_coding_run` means
/// `PipelineRunner::run` never reached `finalize()` — no `RunComplete`/`RunPaused` was emitted,
/// so the `run_events` bridge's own cleanup (`registry.remove` on those events) never fires,
/// leaking this run's token/pause entry, and — if the failure happened AFTER a successful DB row
/// create/reset (e.g. a downstream setup error, not the row-create itself) — the row is left at
/// whatever status was last written (often still `running`/`queued`), unresumable because
/// nothing else will ever retry it. Clean up both here: remove the registry entry directly
/// (mirrors the bridge's own cleanup, just reached via a different exit path) and reset the row
/// to `interrupted` so a later `resume_run` (or the boot-time `reset_stale_running` sweep) can
/// still try again — best-effort, since the row may never have been created at all (an error
/// before `pipeline_runs::create_resumable`, e.g. repo/workspace resolution). Extracted as its
/// own function so the two outcomes (row exists and stuck live vs. no row at all) are directly
/// unit-testable without forcing a genuine failure through the full orchestrator/LLM stack.
async fn reconcile_failed_launch(db: &DbHandle, registry: &RunControlRegistry, run_id: &str) {
    registry.remove(run_id);
    match haily_db::queries::pipeline_runs::get(db, run_id).await {
        Ok(Some(row)) if matches!(row.status.as_str(), "queued" | "running") => {
            if let Err(e) = haily_db::queries::pipeline_runs::transition(
                db,
                run_id,
                haily_db::queries::pipeline_runs::RunTransition {
                    stage_index: row.stage_index,
                    status: "interrupted",
                    attempt: row.attempt,
                    attempts_remaining: row.attempts_remaining,
                    tier_used: row.tier_used.as_deref(),
                    backend_used: row.backend_used.as_deref(),
                    egress: row.egress.as_deref(),
                    gate_output_digest: row.gate_output_digest.as_deref(),
                    pause_reason_class: None,
                },
            )
            .await
            {
                tracing::warn!(run_id, "failed-launch row cleanup transition failed: {e:#}");
            }
        }
        // Already paused/interrupted/terminal (a genuine setup failure raced a stage outcome),
        // or the row never existed (a pre-`create_resumable` setup failure) — nothing to
        // reconcile.
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(run_id, "failed-launch row lookup failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::queries::pipeline_runs;

    async fn db() -> (tempfile::TempDir, Arc<DbHandle>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());
        (dir, db)
    }

    async fn new_session(db: &DbHandle) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        haily_db::queries::sessions::create_session(db, &id, "coding", None)
            .await
            .unwrap();
        id
    }

    /// A run left `running` by a setup failure (never reached `finalize()`) is reset to
    /// `interrupted` — resumable by a later `resume_run`/boot sweep — and its registry entry
    /// (token + pause flag) is removed so it can never leak.
    #[tokio::test]
    async fn reconcile_failed_launch_resets_a_stuck_running_row_and_clears_the_registry() {
        let (_dir, db) = db().await;
        let session = new_session(&db).await;
        let run = pipeline_runs::create(&db, &session, None, 5).await.unwrap();
        pipeline_runs::transition(
            &db,
            &run.id,
            pipeline_runs::RunTransition {
                stage_index: 1,
                status: "running",
                attempt: 2,
                attempts_remaining: 4,
                tier_used: None,
                backend_used: None,
                egress: None,
                gate_output_digest: None,
                pause_reason_class: None,
            },
        )
        .await
        .unwrap();

        let registry = RunControlRegistry::new();
        registry.register(
            &run.id,
            CancellationToken::new(),
            Arc::new(AtomicBool::new(false)),
        );

        reconcile_failed_launch(&db, &registry, &run.id).await;

        let after = pipeline_runs::get(&db, &run.id).await.unwrap().unwrap();
        assert_eq!(
            after.status, "interrupted",
            "must be reset to interrupted, not left running"
        );
        assert_eq!(
            after.stage_index, 1,
            "must preserve the stage the run was actually at"
        );
        assert!(
            !registry.cancel(&run.id),
            "the registry entry must be removed — no leaked token/pause pair"
        );
    }

    /// A setup failure BEFORE any `pipeline_runs` row was ever created (e.g. repo resolution) is
    /// a clean no-op on the DB side — registry cleanup still fires (never a leak either way).
    #[tokio::test]
    async fn reconcile_failed_launch_on_a_never_created_row_is_a_safe_no_op() {
        let (_dir, db) = db().await;
        let registry = RunControlRegistry::new();
        let fake_run_id = uuid::Uuid::new_v4().to_string();
        registry.register(
            &fake_run_id,
            CancellationToken::new(),
            Arc::new(AtomicBool::new(false)),
        );

        reconcile_failed_launch(&db, &registry, &fake_run_id).await;

        assert!(pipeline_runs::get(&db, &fake_run_id)
            .await
            .unwrap()
            .is_none());
        assert!(
            !registry.cancel(&fake_run_id),
            "registry entry must still be removed"
        );
    }

    /// A row already terminal/paused by the time the setup failure is handled (a race, not the
    /// common case) must NOT be clobbered back to `interrupted` — only a genuinely stuck
    /// `queued`/`running` row is reset.
    #[tokio::test]
    async fn reconcile_failed_launch_never_overwrites_an_already_terminal_row() {
        let (_dir, db) = db().await;
        let session = new_session(&db).await;
        let run = pipeline_runs::create(&db, &session, None, 5).await.unwrap();
        pipeline_runs::transition(
            &db,
            &run.id,
            pipeline_runs::RunTransition {
                stage_index: 0,
                status: "done",
                attempt: 0,
                attempts_remaining: 5,
                tier_used: None,
                backend_used: None,
                egress: None,
                gate_output_digest: None,
                pause_reason_class: None,
            },
        )
        .await
        .unwrap();
        let registry = RunControlRegistry::new();

        reconcile_failed_launch(&db, &registry, &run.id).await;

        let after = pipeline_runs::get(&db, &run.id).await.unwrap().unwrap();
        assert_eq!(
            after.status, "done",
            "an already-terminal row must never be overwritten"
        );
    }
}
