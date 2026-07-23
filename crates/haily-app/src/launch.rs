//! App-layer entrypoint for a live coding-pipeline launch (Pipeline Activation & Wiring,
//! phase 1). This is the ONLY place `haily-core`'s launcher (which knows nothing about
//! adapters) meets `haily-io`'s `AdapterManager` — via the shared [`run_control::spawn_launch`]
//! helper (Unified Chat UI phase 6, D3), which wires the `RunEvent` bridge (Seam 2), the
//! distillation bridge (Seam 3), and the run-control registry to a live run, then drives
//! `Orchestrator::launch_coding_run` as a task tracked on the SAME `TaskTracker` the dispatch
//! loop uses, so a long-running build/verify cycle never blocks intake and is still drained by
//! `AppHandle::shutdown`.
use crate::bootstrap::AppHandle;
use crate::run_control::{self, LaunchCtx};
use haily_core::CodingRunSpec;
use haily_types::ResponseChunk;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Launch a coding-pipeline run bound to `resp_tx` (the originating turn's response channel).
///
/// `spec.run_id` is pre-generated here (mirrors `trigger::launch`'s own pre-generation) so
/// `spawn_launch` can register it into `app.run_control` SYNCHRONOUSLY, before the tracked task
/// spawns — a `kill_run` issued the instant after this function returns still has a token to
/// cancel. `run_cancel` is a fresh child of the root shutdown token (mirrors `dispatch_loop`'s
/// own per-turn token): cancelling the root cancels this launch's run too, so a pending approval
/// denies immediately on shutdown instead of holding up the drain.
pub fn launch_coding_run(
    app: &AppHandle,
    mut spec: CodingRunSpec,
    resp_tx: mpsc::Sender<ResponseChunk>,
) {
    spec.run_id = Uuid::new_v4();
    let run_cancel = app.shutdown.child_token();
    let ctx = LaunchCtx {
        orc: Arc::clone(&app.orchestrator),
        am: app.adapters.clone(),
        tasks: app.tasks.clone(),
        db: Arc::clone(&app.db),
        registry: app.run_control_registry(),
    };
    run_control::spawn_launch(ctx, spec, run_cancel, resp_tx);
}
