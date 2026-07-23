//! Request dispatch loop — pulls requests from adapter channels, fans out to the
//! orchestrator, and forwards response chunks back to the originating adapter.
use crate::slash_registry::SlashRegistry;
use crate::trigger::{self, TriggerAction};
use crate::turns::TurnRegistry;
use anyhow::Result;
use haily_core::{Orchestrator, RunKind};
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
    slash_registry: Arc<SlashRegistry>,
) -> Result<()> {
    let (req_tx, req_rx) = mpsc::channel::<Request>(64);
    am.start_all(req_tx).await?;
    info!("dispatch loop running");

    let dispatch_tasks = tasks.clone();
    tasks.spawn(dispatch_loop(
        am,
        orc,
        req_rx,
        shutdown,
        dispatch_tasks,
        turns,
        slash_registry,
    ));
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
    slash_registry: Arc<SlashRegistry>,
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
        // Cloned here (not moved) so a Launch/ConfirmThenLaunch branch below can hand its own
        // owned `TaskTracker` to `trigger::launch`'s bridge spawns — the outer `tasks` binding
        // stays available for every subsequent loop iteration exactly as before this phase.
        let tasks_for_launch = tasks.clone();
        // Per-turn child of the root shutdown token: cancelling the root cancels every
        // in-flight turn's token too, so a pending tool approval (ApprovalBroker::request)
        // denies immediately on shutdown instead of blocking the drain for up to 120s.
        let turn_cancel = shutdown.child_token();
        // Registered by session_id so a UI-facing "Stop" action (haily-app::TurnRegistry)
        // can fire this exact turn's token on demand, independent of the root shutdown
        // token above.
        turns.register(session_id, turn_cancel.clone());
        let turns_clone = Arc::clone(&turns);
        // Unified Chat UI phase 2: cloned per-iteration like `orc_clone`/`am_clone` — the
        // registry itself is a cheap `Arc<RwLock<..>>` snapshot holder, shared (not rebuilt)
        // across every request.
        let registry_clone = Arc::clone(&slash_registry);

        tasks.spawn(async move {
            // Re-bind mutable: `req` needs a mutable borrow below (a `SkillTurn` slash command
            // tags `forced_skill` on it) but every other branch still moves the original value
            // where it needs to (the `NormalTurn` and `ConfirmThenLaunch`-denied-fallback paths).
            let mut req = req;
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

            // Lazy rebuild (P02↔P08 interop contract): cheap no-op unless the authored-skill
            // kit-pack version has moved since the last build.
            registry_clone.ensure_fresh(&orc_clone.kms, &orc_clone.db).await;

            // Pipeline Activation & Wiring phase 2: decide whether this request is a normal
            // chat turn, an explicit slash-triggered pipeline launch, or a confirm-gated
            // chat-intent launch — `resolve` mutates `req` only for a `SkillTurn` slash command,
            // so every branch below can still move the original `req` where it needs to (the
            // `NormalTurn` and `ConfirmThenLaunch`-denied-fallback paths both need it).
            match trigger::resolve(&mut req, &registry_clone) {
                TriggerAction::NormalTurn => {
                    trigger::run_normal_turn(&orc_clone, req, resp_tx, turn_cancel).await;
                }
                TriggerAction::LaunchPlan(task) => {
                    let depth = req.depth;
                    let handles = trigger::LaunchHandles {
                        orc: orc_clone,
                        am: am_clone.clone(),
                        tasks: tasks_for_launch,
                    };
                    trigger::launch(handles, turn_cancel, RunKind::Plan, task, session_id, depth, resp_tx);
                }
                TriggerAction::LaunchBuild(task) => {
                    let depth = req.depth;
                    let handles = trigger::LaunchHandles {
                        orc: orc_clone,
                        am: am_clone.clone(),
                        tasks: tasks_for_launch,
                    };
                    trigger::launch(handles, turn_cancel, RunKind::Build, task, session_id, depth, resp_tx);
                }
                TriggerAction::ConfirmThenLaunch(kind, task) => {
                    let handles = trigger::LaunchHandles {
                        orc: orc_clone,
                        am: am_clone.clone(),
                        tasks: tasks_for_launch,
                    };
                    trigger::confirm_then_launch(handles, turn_cancel, kind, task, req, resp_tx).await;
                }
                TriggerAction::PromptTask(kind) => {
                    let hint = trigger::task_prompt_hint(kind);
                    resp_tx.send(ResponseChunk::Text(hint)).await.ok();
                    resp_tx.send(ResponseChunk::Complete).await.ok();
                }
                TriggerAction::UnknownSlashHint(name) => {
                    let hint = haily_io::slash::unknown_hint(&name);
                    resp_tx.send(ResponseChunk::Text(hint)).await.ok();
                    resp_tx.send(ResponseChunk::Complete).await.ok();
                }
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
