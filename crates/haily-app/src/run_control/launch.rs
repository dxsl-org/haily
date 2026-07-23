//! The ONE launch path `haily-app`'s `launch.rs` (GUI "New run") and `trigger.rs` (slash/chat-
//! intent) both call (Unified Chat UI phase 6, D3) — replaces the bridge-wiring + task-spawn
//! each used to duplicate. Registering the run's cancel/pause handles happens SYNCHRONOUSLY here,
//! before the tracked task is spawned, so a `kill_run` issued the instant after this function
//! returns still has a token to cancel.
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
            let _ = resp_tx
                .send(ResponseChunk::Error(format!("⚠️ {e:#}")))
                .await;
            let _ = resp_tx.send(ResponseChunk::Complete).await;
        }
    });
}
