//! Request dispatch loop — pulls requests from adapter channels, fans out to the
//! orchestrator, and forwards response chunks back to the originating adapter.
use crate::turns::TurnRegistry;
use anyhow::Result;
use haily_core::Orchestrator;
use haily_io::AdapterManager;
use haily_types::{Request, ResponseChunk};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::info;

/// Spawn the dispatch loop as a tracked background task and return once it is running.
///
/// The loop itself runs for the lifetime of the app; this function only awaits
/// `AdapterManager::start_all` (which starts each adapter's own event loop) before
/// returning — it does not block on the dispatch loop finishing.
pub async fn spawn_dispatch_loop(
    am: AdapterManager,
    orc: Arc<Orchestrator>,
    shutdown: CancellationToken,
    tasks: TaskTracker,
    turns: Arc<TurnRegistry>,
) -> Result<()> {
    let (req_tx, req_rx) = mpsc::channel::<Request>(64);
    am.start_all(req_tx).await?;
    info!("dispatch loop running");

    let dispatch_tasks = tasks.clone();
    tasks.spawn(dispatch_loop(am, orc, req_rx, shutdown, dispatch_tasks, turns));
    Ok(())
}

/// Receive requests and dispatch each to the orchestrator concurrently.
///
/// Each request gets its own response channel; chunks are forwarded to the
/// originating adapter via `AdapterManager` as they arrive. The per-turn task is
/// spawned on `tasks` (not bare `tokio::spawn`) — otherwise `AppHandle::shutdown`
/// would drain the watcher/daemon tasks while silently abandoning whatever turn was
/// in flight, which defeats the entire point of a drain.
async fn dispatch_loop(
    am: AdapterManager,
    orc: Arc<Orchestrator>,
    mut req_rx: mpsc::Receiver<Request>,
    shutdown: CancellationToken,
    tasks: TaskTracker,
    turns: Arc<TurnRegistry>,
) {
    loop {
        let req = tokio::select! {
            _ = shutdown.cancelled() => {
                info!("dispatch loop shutting down");
                break;
            }
            req = req_rx.recv() => match req {
                Some(req) => req,
                None => break,
            },
        };

        // When both select arms are ready (a request is buffered as cancellation fires),
        // tokio may pick the recv arm — re-check so shutdown never spawns a fresh turn
        // that would eat into the drain budget.
        if shutdown.is_cancelled() {
            break;
        }

        let session_id = req.session_id;
        am.bind_session(session_id, &req.adapter_id);

        let (resp_tx, mut resp_rx) = mpsc::channel::<ResponseChunk>(256);
        let orc_clone = Arc::clone(&orc);
        let am_clone = am.clone();
        // Per-turn child of the root shutdown token: cancelling the root cancels every
        // in-flight turn's token too, so a pending tool approval (ApprovalBroker::request)
        // denies immediately on shutdown instead of blocking the drain for up to 120s.
        let turn_cancel = shutdown.child_token();
        // Registered by session_id so a UI-facing "Stop" action (haily-app::TurnRegistry)
        // can fire this exact turn's token on demand, independent of the root shutdown
        // token above.
        turns.register(session_id, turn_cancel.clone());
        let turns_clone = Arc::clone(&turns);

        tasks.spawn(async move {
            // Forward chunks from orchestrator → adapter while the agent loop runs.
            let delivery = {
                let am = am_clone.clone();
                tokio::spawn(async move {
                    while let Some(chunk) = resp_rx.recv().await {
                        let done = matches!(chunk, ResponseChunk::Complete);
                        am.deliver(session_id, chunk).await.ok();
                        if done {
                            break;
                        }
                    }
                })
            };

            let resp_tx_err = resp_tx.clone();
            if let Err(e) = orc_clone.process(req, resp_tx, turn_cancel).await {
                tracing::error!("orchestrator error: {e:#}");
                // `Error`, not `Text` — a turn that streamed partial text before
                // failing (e.g. a mid-stream `StreamChunk::Error` from the LLM) must
                // let buffering adapters (Telegram) tell "discard what's buffered"
                // apart from "append this too", or the user sees a single fused
                // "partial-answer⚠️error" message. See `haily_types::ResponseChunk::Error`.
                resp_tx_err.send(ResponseChunk::Error(format!("⚠️ {e:#}"))).await.ok();
                resp_tx_err.send(ResponseChunk::Complete).await.ok();
            }

            // INVARIANT: `delivery` is a bare `tokio::spawn`, so it is only drained by
            // this `.await`. Every exit path from this tracked task MUST reach here —
            // do not add an early `return`/`?` between the spawn above and this await, or
            // the forwarder leaks untracked (holding resp_rx + the session binding).
            delivery.await.ok();
            am_clone.unbind_session(&session_id);
            // Every turn-exit path (success, orchestrator error, or a mid-turn Stop)
            // funnels through here, so this is the one place the registry entry needs
            // cleaning up — a `Stop` click already removed it via `cancel()`, so this
            // is a no-op then; on normal/error completion it prevents the map growing
            // unbounded with entries for turns that already finished.
            turns_clone.remove(session_id);
        });
    }
}
