//! App-layer entrypoint for a live coding-pipeline launch (Pipeline Activation & Wiring,
//! phase 1). This is the ONLY place `haily-core`'s launcher (which knows nothing about
//! adapters) meets `haily-io`'s `AdapterManager` — it wires the `RunEvent` bridge (Seam 2) and
//! the distillation bridge (Seam 3) to a live run, then drives `Orchestrator::launch_coding_run`
//! as a task tracked on the SAME `TaskTracker` the dispatch loop uses, so a long-running
//! build/verify cycle never blocks intake and is still drained by `AppHandle::shutdown`.
//!
//! Not yet called from any live trigger (that is P2's slash/chat-intent triggers and P3's GUI
//! "New run" surface) — this phase ships the entrypoint building block those phases wire up.

use crate::bootstrap::AppHandle;
use crate::watchers::{spawn_distillation_bridge, spawn_run_event_bridge};
use haily_core::CodingRunSpec;
use haily_types::ResponseChunk;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Bounded capacity for one run's `RunEvent` channel — generous headroom above a typical run's
/// event count (mirrors the eval harness's own `1024`), so a burst of stage output never blocks
/// the runner mid-stage waiting on a slow adapter drain.
const RUN_EVENTS_CAPACITY: usize = 1024;
/// Bounded capacity for one run's distillation-proposal channel — at most a handful of
/// proposals surface per run (Ship-stage-only emission), so a small buffer is ample.
const DISTILLATION_CAPACITY: usize = 16;

/// Launch a coding-pipeline run bound to `resp_tx` (the originating turn's response channel).
///
/// Mirrors `dispatch::dispatch_loop`'s own per-turn task contract: `resp_tx` receives a
/// terminal `ResponseChunk::Complete` exactly once — on success `Orchestrator::launch_coding_run`
/// sends it itself (mirrors `agent::run_turn`'s own contract); on a setup failure this function
/// sends `Error` + `Complete`, mirroring `dispatch_loop`'s own error path for `orc.process()`.
/// The caller's existing delivery-drain task therefore needs no special-casing for a
/// pipeline-originated turn.
pub fn launch_coding_run(
    app: &AppHandle,
    spec: CodingRunSpec,
    resp_tx: mpsc::Sender<ResponseChunk>,
) {
    let (events_tx, events_rx) = mpsc::channel(RUN_EVENTS_CAPACITY);
    let (dist_tx, dist_rx) = mpsc::channel(DISTILLATION_CAPACITY);
    let session_id = spec.session_id;
    // A child of the root shutdown token (mirrors `dispatch_loop`'s per-turn token): cancelling
    // the root cancels this launch's run too, so a pending approval denies immediately on
    // shutdown instead of holding up the drain.
    let run_cancel = app.shutdown.child_token();

    spawn_run_event_bridge(
        session_id,
        events_rx,
        app.adapters.clone(),
        Arc::clone(&app.db),
        run_cancel.clone(),
        app.tasks.clone(),
    );
    spawn_distillation_bridge(
        dist_rx,
        app.adapters.clone(),
        run_cancel.clone(),
        app.tasks.clone(),
    );

    let orc = Arc::clone(&app.orchestrator);
    app.tasks.clone().spawn(async move {
        let result = orc
            .launch_coding_run(spec, resp_tx.clone(), events_tx, Some(dist_tx), run_cancel)
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
