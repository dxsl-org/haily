//! Dispatch-layer trigger resolver (Pipeline Activation & Wiring, phase 2): decides whether an
//! incoming [`Request`] is a normal chat turn, an explicit slash-triggered pipeline launch, or a
//! confirm-gated chat-intent launch, and drives the confirm + launch flow for the latter cases.
//! Sits between `dispatch_loop`'s `recv` and `orc.process` so a launch or a confirm-prompt never
//! enters the LLM turn machinery.
//!
//! [`resolve`] is synchronous (cheap, no I/O). [`confirm_then_launch`]'s `.await` on the approval
//! broker runs inside the CALLER's already-spawned per-turn task, never the main `dispatch_loop`
//! receive loop, so a pending confirm never blocks intake of the next request.
//!
//! [`launch`] cannot call `haily_app::launch_coding_run(&AppHandle, ..)` (Phase 1's entrypoint)
//! directly: `dispatch_loop` is spawned INSIDE `AppHandle::bootstrap`, before `AppHandle` itself
//! is constructed — there is no `&AppHandle` to hand it at this call site. [`launch`] instead
//! reimplements `launch.rs::launch_coding_run`'s bridge-wiring over the raw handles
//! (`AdapterManager`, `Arc<Orchestrator>`, `TaskTracker`, [`CancellationToken`]) `dispatch_loop`
//! already owns. Deliberate difference: the cancel token is the CALLER's already-registered
//! per-turn `turn_cancel`, not a fresh child of the root shutdown token — so a chat-triggered
//! launch stays cancellable by the same "Stop" action a normal turn already is. See the phase's
//! Deviation Log.
use crate::slash_registry::SlashRegistry;
use haily_core::{classify_coding_intent, CodingRunSpec, Orchestrator, RunKind};
use haily_db::DbHandle;
use haily_io::{slash, AdapterManager};
use haily_types::{DepthMode, Request, RequestOrigin, ResponseChunk};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

/// Mirrors `launch.rs::RUN_EVENTS_CAPACITY` — generous headroom above a typical run's event
/// count so a stage-output burst never blocks the runner mid-stage waiting on a slow adapter.
const RUN_EVENTS_CAPACITY: usize = 1024;
/// Mirrors `launch.rs::DISTILLATION_CAPACITY` — at most a handful of proposals per run.
const DISTILLATION_CAPACITY: usize = 16;

/// The three raw handles `launch`/`confirm_then_launch` need, bundled to keep both functions'
/// argument count under clippy's `too_many_arguments` threshold (mirrors `pipeline::LaunchDeps`'s
/// own "bundle the caller's cloned handles into a plain struct" convention).
pub struct LaunchHandles {
    pub orc: Arc<Orchestrator>,
    pub am: AdapterManager,
    pub tasks: TaskTracker,
    /// Threaded into `spawn_run_event_bridge` (Unified Chat UI phase 5, D2) so this run's
    /// events persist. Callers extract it from `orc.db` before moving `orc` into this struct.
    pub db: Arc<DbHandle>,
}

/// What a dispatch-layer trigger decides to do with one incoming [`Request`].
#[derive(Debug)]
pub enum TriggerAction {
    /// No launch — route to the orchestrator's normal turn (`orc.process`).
    NormalTurn,
    /// `/plan <task>` — launch the plan-only pipeline immediately (explicit user command).
    LaunchPlan(String),
    /// `/code` or `/build <task>` — launch the build-only pipeline immediately.
    LaunchBuild(String),
    /// A chat-shaped coding request with no explicit slash — launch ONLY after the user
    /// approves the run-launch confirm prompt (Security Considerations: this IS the RiskTier
    /// boundary for an auto-detected launch).
    ConfirmThenLaunch(RunKind, String),
    /// A registered coding slash (`/plan`, `/code`, `/build`) with no task argument — prompt for
    /// one instead of launching.
    PromptTask(RunKind),
    /// A slash command that does not resolve to any registered name — deliver the hint text,
    /// never silently swallow it.
    UnknownSlashHint(String),
}

/// Decide the trigger action for one incoming request. Parses a leading slash command via
/// `slash::parse` and resolves it against the data-driven [`SlashRegistry`] (Unified Chat UI
/// phase 2, D1) — built-ins map to the same actions `resolve_slash` always produced; an
/// authored/synthesized skill command tags `req.forced_skill` and routes as a normal turn (see
/// `slash_registry::resolve`). For a no-slash [`RequestOrigin::Chat`] message, classifies chat
/// intent via `classify_coding_intent`. `Cli`-origin requests never reach the intent classifier
/// here (SEC-H: `Cli` is the eval bypass path and must stay unreachable from a chat message);
/// the classifier itself re-checks the same gate as a defense-in-depth mirror.
///
/// `req` is mutated ONLY when the slash resolves to a `SkillTurn` action — every other branch
/// leaves it untouched, so a caller does not need to guard against surprise mutation.
pub fn resolve(req: &mut Request, registry: &SlashRegistry) -> TriggerAction {
    if let Some((name, arg)) = slash::parse(&req.message) {
        return crate::slash_registry::resolve::resolve(req, &name, &arg, registry);
    }

    if req.origin == RequestOrigin::Chat {
        if let Some(kind) = classify_coding_intent(&req.message, req.origin) {
            return TriggerAction::ConfirmThenLaunch(kind, req.message.clone());
        }
    }

    TriggerAction::NormalTurn
}

/// User-facing text for [`TriggerAction::PromptTask`] — deterministic (never routed through the
/// LLM), so the test asserting this behavior does not depend on model output.
pub fn task_prompt_hint(kind: RunKind) -> String {
    let (cmd, verb) = match kind {
        RunKind::Plan => ("/plan", "plan"),
        RunKind::Build | RunKind::PlanThenBuild => ("/code", "build"),
    };
    format!("Send `{cmd} <task description>` to tell me what to {verb} — I need a task to launch a run.")
}

/// Run a normal orchestrator turn, mirroring `dispatch_loop`'s own pre-phase-2 error handling
/// exactly (shared here so the `ConfirmThenLaunch`-denied fallback does not duplicate it).
pub async fn run_normal_turn(
    orc: &Orchestrator,
    req: Request,
    resp_tx: mpsc::Sender<ResponseChunk>,
    cancel: CancellationToken,
) {
    let resp_tx_err = resp_tx.clone();
    if let Err(e) = orc.process(req, resp_tx, cancel).await {
        tracing::error!("orchestrator error: {e:#}");
        // `Error`, not `Text` — see `dispatch_loop`'s own comment on this exact pattern.
        resp_tx_err
            .send(ResponseChunk::Error(format!("⚠️ {e:#}")))
            .await
            .ok();
        resp_tx_err.send(ResponseChunk::Complete).await.ok();
    }
}

/// Request run-launch confirmation through the session's EXISTING `ApprovalGate` broker — the
/// SAME broker + `ResponseChunk::ToolApprovalRequest` UX a normal tool-approval uses (no parallel
/// confirm mechanism, Requirements). On approve, spawn the launch; on deny, timeout, or a closed
/// origin channel, fall through to a normal chat turn (never launches with no explicit approval).
pub async fn confirm_then_launch(
    handles: LaunchHandles,
    turn_cancel: CancellationToken,
    kind: RunKind,
    task: String,
    req: Request,
    resp_tx: mpsc::Sender<ResponseChunk>,
) {
    let session_id = req.session_id;
    let approved = confirm(
        &handles.orc,
        &resp_tx,
        session_id,
        &turn_cancel,
        kind,
        &task,
    )
    .await;
    if approved {
        let depth = req.depth;
        launch(handles, turn_cancel, kind, task, session_id, depth, resp_tx);
    } else {
        run_normal_turn(&handles.orc, req, resp_tx, turn_cancel).await;
    }
}

async fn confirm(
    orc: &Orchestrator,
    resp_tx: &mpsc::Sender<ResponseChunk>,
    session_id: Uuid,
    cancel: &CancellationToken,
    kind: RunKind,
    task: &str,
) -> bool {
    let approval_id = Uuid::new_v4();
    let tool = match kind {
        RunKind::Plan => "run_plan",
        RunKind::Build => "run_build",
        RunKind::PlanThenBuild => "run_plan_then_build",
    };
    let sent = resp_tx
        .send(ResponseChunk::ToolApprovalRequest {
            tool: tool.to_string(),
            args: task.to_string(),
            approval_id,
            origin: None,
            reversible: false,
        })
        .await;
    if sent.is_err() {
        // Origin channel already closed — nothing to confirm against; deny safely rather than
        // launch with no observable prompt.
        return false;
    }
    orc.approval_gate()
        .request(approval_id, session_id, cancel)
        .await
}

/// Launch one coding-pipeline run bound to `resp_tx`, mirroring `haily_app::launch_coding_run`'s
/// own bridge-wiring (module doc explains why this cannot call that function directly).
/// `run_cancel` is spawned as `tasks`'s own task — the caller returns immediately; this task owns
/// the run to completion and sends the terminal `ResponseChunk::Complete` itself (mirrors
/// `Orchestrator::launch_coding_run`'s contract), so the caller's existing delivery-drain task
/// needs no special-casing for a pipeline-originated turn.
pub fn launch(
    handles: LaunchHandles,
    run_cancel: CancellationToken,
    kind: RunKind,
    task: String,
    session_id: Uuid,
    depth: DepthMode,
    resp_tx: mpsc::Sender<ResponseChunk>,
) {
    let LaunchHandles { orc, am, tasks, db } = handles;
    let spec = CodingRunSpec {
        kind,
        task,
        session_id,
        work_item_id: None,
        repo_path: None,
        depth,
    };

    let (events_tx, events_rx) = mpsc::channel(RUN_EVENTS_CAPACITY);
    let (dist_tx, dist_rx) = mpsc::channel(DISTILLATION_CAPACITY);

    crate::watchers::spawn_run_event_bridge(
        session_id,
        events_rx,
        am.clone(),
        db,
        run_cancel.clone(),
        tasks.clone(),
    );
    crate::watchers::spawn_distillation_bridge(dist_rx, am, run_cancel.clone(), tasks.clone());

    tasks.spawn(async move {
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

#[cfg(test)]
mod tests;
