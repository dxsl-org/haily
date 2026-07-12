//! Coding eval runner (Sub-Agent + Skill Architecture phase 9) — `haily eval coding`.
//!
//! Creates a THROWAWAY [`CodingWorkspace`] per fixture task, drives the full plan→build→verify
//! pipeline headless with the eval-mode privileges, and scores the result by deterministic gates
//! (NOT an LLM judge — locked). The result is persisted to `eval_runs` + rendered to a report.
//!
//! ## Two-tier deliverable (honest split, mirrors P0's capability-spike honesty)
//! - The pipeline behavior + eval-mode invariants (SEC-H origin gate, ship hard-block,
//!   IrreversibleWrite→deny, reproducible scoring) are exercised by scripted-LLM goldens
//!   (`crates/haily-core/tests/coding_goldens.rs`) that run in CI, zero network.
//! - The BASELINE MATRIX RUN (local × {Normal, Deep} × escalation {off,on}) drives real models
//!   and needs a configured local/cloud model host NOT present in this build env. The runner +
//!   fixtures + scoring + persistence are BUILT here; the matrix run itself is a documented
//!   manual step (see docs/project-roadmap.md). An env-gated `#[ignore]` test exercises the
//!   runner end-to-end only when `HAILY_EVAL_MODEL` is set.
//!
//! ## SEC-H — structural request-origin gate
//! [`EvalMode`] (which carries the privileged plan-gate auto-approval + ship hard-block) is
//! constructible ONLY from a [`RequestOrigin::Cli`] request. A chat-origin `Request` (the
//! default for every I/O adapter) can NEVER obtain one — proven by
//! [`tests::chat_origin_request_can_never_enable_eval_mode`].

mod manifest;
mod report;
mod scoring;
mod setup;

#[cfg(test)]
mod tests;

pub use manifest::{parse_task_yaml, TaskManifest};
pub use report::{render_outcome, render_report};
pub use scoring::{score, GateResult, ScoreInputs, ScoreResult};

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::Result;
use haily_db::queries::eval_runs::{self, NewEvalRun};
use haily_db::queries::pipeline_runs;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::LlmRouter;
use haily_tools::coding::workspace::CodingWorkspace;
use haily_tools::ToolRegistry;
use haily_types::{ApprovalGate, DepthMode, Request, RequestOrigin, ResponseChunk, RunEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::approval::ApprovalBroker;
use crate::pipeline::build_pipeline::{run_build, BuildRunSpec, EmitFindingsTool, PhaseInput};
use crate::pipeline::plan_pipeline::{run_plan, EmitPlanDraftTool, PlanRunSpec, RenderPlanTool};
use crate::pipeline::runner::PipelineRunner;

/// Coding tools an eval stage may use — the plan/build surface MINUS the two ship tools
/// (`worktree_apply`, `git_commit`). Omitting them from the eval base registry is the STRUCTURAL
/// ship hard-block (SEC-H): the ship stage's whitelist snapshots from this registry, so it
/// resolves to an empty tool set and the model literally cannot apply to a real repo.
const EVAL_ALLOWED_TOOLS: &[&str] = &[
    "fs_read", "fs_list", "fs_grep", "fs_write", "fs_edit", "fs_move", "fs_delete", "shell_exec",
    "git_status", "git_diff",
];

/// Privileged eval-mode witness (red-team SEC-H). Its presence authorizes the plan-gate
/// auto-approval + ship hard-block. Constructible ONLY from a CLI-origin request — a chat
/// `Request` (every adapter's default origin) can never mint one, so eval mode is structurally
/// unreachable from chat.
#[derive(Debug, Clone, Copy)]
pub struct EvalMode {
    _private: (),
}

impl EvalMode {
    /// Yield eval-mode privileges ONLY for a CLI-transport origin. `RequestOrigin::Chat` (the
    /// default for every I/O adapter) returns `None` — the privileged plan-gate bypass + ship
    /// hard-block are unreachable from any chat/remote request.
    pub fn from_origin(origin: RequestOrigin) -> Option<EvalMode> {
        match origin {
            RequestOrigin::Cli => Some(EvalMode { _private: () }),
            RequestOrigin::Chat => None,
        }
    }

    /// Convenience over [`EvalMode::from_origin`] for a whole [`Request`].
    pub fn from_request(req: &Request) -> Option<EvalMode> {
        Self::from_origin(req.origin)
    }
}

/// Shared handles the coding eval drives against. Grouped so [`run_coding_eval`] stays within a
/// sane arity. `llm` is the SAME `Arc<RwLock<Arc<LlmRouter>>>` shape the orchestrator holds, so a
/// scripted router can be injected in tests and a real router in production.
pub struct EvalDeps {
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    pub llm: Arc<RwLock<Arc<LlmRouter>>>,
    /// Model name recorded on the `eval_runs` row (a GGUF name or cloud model id).
    pub model: String,
    /// Tier configuration label (`local` / `local+escalate` / `cloud`).
    pub tier_config: String,
    /// P3 escalation policy for this baseline arm (the `{off,on}` matrix axis).
    pub escalation_enabled: bool,
}

/// A per-attempt egress tag (FMA-M2). `egress` is `local` / `cloud` / `unknown` — a coarse map
/// from the attempt's resolved tier (local llama ceiling = medium; thinking/ultra ⇒ cloud). A
/// usage-returning backend will later replace the heuristic with the confirmed backend.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EgressTag {
    pub attempt: u64,
    pub tier: String,
    pub egress: String,
}

/// The scored, telemetry-bearing outcome of one eval task run.
pub struct EvalOutcome {
    pub task_id: String,
    pub model: String,
    pub tier_config: String,
    pub depth: String,
    pub score: ScoreResult,
    pub escalation_count: u32,
    pub wall_clock_ms: u64,
    /// Per-attempt egress tags (FMA-M2) — kept even when empty.
    pub egress: Vec<EgressTag>,
    /// Per-stage token records (verbatim runner JSON), for the report + `eval_runs` row.
    pub per_stage_tokens: Vec<serde_json::Value>,
    /// Set once persisted.
    pub eval_run_id: Option<String>,
}

impl EvalOutcome {
    /// A one-line egress summary for the report (e.g. `local×3, cloud×1`).
    pub fn egress_summary(&self) -> String {
        if self.egress.is_empty() {
            return "none recorded".to_string();
        }
        let mut local = 0;
        let mut cloud = 0;
        let mut unknown = 0;
        for t in &self.egress {
            match t.egress.as_str() {
                "local" => local += 1,
                "cloud" => cloud += 1,
                _ => unknown += 1,
            }
        }
        let mut parts = Vec::new();
        if local > 0 {
            parts.push(format!("local×{local}"));
        }
        if cloud > 0 {
            parts.push(format!("cloud×{cloud}"));
        }
        if unknown > 0 {
            parts.push(format!("unknown×{unknown}"));
        }
        parts.join(", ")
    }
}

/// Run one coding eval task end-to-end: throwaway workspace → full pipeline (eval mode) → gate
/// scoring → persisted `eval_runs` row. `fixture_src` is the committed fixture dir under
/// `evals/fixtures/<id>` (copied per run — never mutated). `_mode` is the SEC-H witness proving a
/// CLI origin; its presence is the authorization to run with the privileged plan-gate bypass.
///
/// # Errors
/// Returns an error only for a setup failure (throwaway staging, workspace open, or persistence);
/// a failing gate / paused pipeline is a normal scored outcome, not an error.
pub async fn run_coding_eval(
    deps: &EvalDeps,
    manifest: &TaskManifest,
    fixture_src: &std::path::Path,
    depth: DepthMode,
    _mode: EvalMode,
) -> Result<EvalOutcome> {
    let session_id = Uuid::new_v4();
    haily_db::queries::sessions::create_session(&deps.db, &session_id.to_string(), "eval", None)
        .await?;

    let original_hash = setup::tree_hash(fixture_src).await?;
    let (_repo_holder, repo) = setup::stage_throwaway_repo(fixture_src).await?;
    let worktrees_holder = tempfile::tempdir()?;
    let workspace =
        CodingWorkspace::open(&deps.db, &session_id.to_string(), &repo, worktrees_holder.path(), None)
            .await?;

    let base_tools = eval_base_registry(&workspace, &manifest.id, &manifest.description);
    let broker = Arc::new(ApprovalBroker::new());
    let (user_tx, user_rx) = mpsc::channel::<ResponseChunk>(64);
    let (ev_tx, mut ev_rx) = mpsc::channel::<RunEvent>(1024);
    let cancel = CancellationToken::new();
    let responder = spawn_eval_auto_responder(user_rx, Arc::clone(&broker), session_id);

    let started = Instant::now();
    {
        let runner = PipelineRunner::new(
            Arc::clone(&deps.db),
            Arc::clone(&deps.kms),
            Arc::clone(&deps.llm),
            base_tools,
            Arc::clone(&broker) as Arc<dyn ApprovalGate>,
            Arc::new(AtomicBool::new(false)),
            cancel.clone(),
            user_tx,
            ev_tx,
            deps.escalation_enabled,
        );

        // Full pipeline: plan (scout→design→write→approval, auto-approved) then build→verify→ship
        // (ship is structurally hard-blocked — no worktree_apply in the registry). A plan/build
        // failure is a normal scored outcome, so a runner setup error is the only `?` here.
        let plan_spec = PlanRunSpec {
            task: manifest.description.clone(),
            slug: manifest.id.clone(),
            session_id,
            work_item_id: None,
            attempts_budget: 8,
            workspace: &workspace,
            revise_feedback: None,
            depth,
        };
        let _ = run_plan(&runner, &deps.db, plan_spec).await;

        let phase = PhaseInput {
            name: "impl".to_string(),
            tier: None,
            content: manifest.description.clone(),
            target_files: Vec::new(),
        };
        let build_spec = BuildRunSpec {
            phases: vec![phase],
            session_id,
            work_item_id: None,
            attempts_budget: manifest.max_tool_calls.max(8) as i64,
            workspace: &workspace,
            compile: manifest.gate_cmd()?,
            test: manifest.gate_cmd()?,
            depth,
            distillation_tx: None,
        };
        let _ = run_build(&runner, &deps.db, build_spec).await;
        // Runner drops here → its `user_tx` clone drops → the auto-responder's channel closes.
    }
    let wall_clock_ms = started.elapsed().as_millis() as u64;
    let _ = responder.await;

    // Telemetry from the drained event stream (spans both runs — one `ev_tx`).
    let (escalation_count, egress, per_stage_tokens) = drain_telemetry(&mut ev_rx);

    // Deterministic gate scoring.
    let gate = manifest.gate_cmd()?;
    let gate_exit = setup::run_gate_command(&gate.program, &gate.args, workspace.worktree_root()).await?;
    let after_hash = setup::tree_hash(fixture_src).await?;
    let journal_rows = pipeline_runs::count_for_session(&deps.db, &session_id.to_string())
        .await
        .unwrap_or(0)
        .max(0) as usize;
    let score = score(&ScoreInputs {
        gate_exit,
        fixture_original_unchanged: original_hash == after_hash,
        journal_rows,
        // Ship is structurally hard-blocked (no worktree_apply tool) → the real throwaway repo's
        // working tree still exists. If the workspace worktree vanished, an apply ran (breach).
        ship_applied: !workspace.worktree_root().is_dir(),
    });

    let mut outcome = EvalOutcome {
        task_id: manifest.id.clone(),
        model: deps.model.clone(),
        tier_config: deps.tier_config.clone(),
        depth: depth.as_label().to_string(),
        score,
        escalation_count,
        wall_clock_ms,
        egress,
        per_stage_tokens,
        eval_run_id: None,
    };
    outcome.eval_run_id = Some(persist(deps, &outcome).await?);

    // Best-effort teardown (the temp dirs drop regardless).
    let _ = workspace.discard(&deps.db).await;
    Ok(outcome)
}

/// Build the eval base registry: the coding tool surface MINUS ship tools (structural
/// ship-block) + the three synthetic pipeline emitters (per-run, workspace-scoped).
fn eval_base_registry(
    workspace: &CodingWorkspace,
    slug: &str,
    task: &str,
) -> Arc<ToolRegistry> {
    let mut reg = ToolRegistry::build_v1().sub_registry(EVAL_ALLOWED_TOOLS);
    let root = workspace.worktree_root().to_path_buf();
    reg.register(Arc::new(EmitPlanDraftTool::new(root.clone(), slug)));
    reg.register(Arc::new(RenderPlanTool::new(root, slug, task)));
    reg.register(Arc::new(EmitFindingsTool));
    Arc::new(reg)
}

/// Spawn the SCOPED eval auto-responder (FMA-M4). Auto-APPROVES only the pipeline checkpoint
/// (the plan gate — `tool == "pipeline_checkpoint"`) and DENIES every real tool approval (any
/// `IrreversibleWrite` — worktree_apply, an over-cap delete, a connector). An IrreversibleWrite
/// in eval therefore becomes a deterministic Failure, never auto-approved. Exits when the runner
/// drops its `user_tx` (channel close).
fn spawn_eval_auto_responder(
    mut user_rx: mpsc::Receiver<ResponseChunk>,
    broker: Arc<ApprovalBroker>,
    session_id: Uuid,
) -> tokio::task::JoinHandle<()> {
    use haily_types::ApprovalResolver;
    tokio::spawn(async move {
        while let Some(chunk) = user_rx.recv().await {
            if let ResponseChunk::ToolApprovalRequest { tool, approval_id, .. } = chunk {
                // SCOPED: only the plan/ship checkpoint is auto-approved; the ship's real write
                // (worktree_apply) is separately hard-blocked (absent from the registry), so
                // approving the checkpoint can never apply to a real repo. Everything else — any
                // genuine IrreversibleWrite — is denied.
                let approve = tool == "pipeline_checkpoint";
                broker.resolve(approval_id, session_id, approve);
            }
        }
    })
}

/// Reduce the drained `RunEvent` stream to `(escalation_count, egress_tags, per_stage_tokens)`.
/// Egress is derived per stage/attempt from the resolved tier (FMA-M2 schema; coarse mapping
/// until a usage-returning backend confirms the real backend).
fn drain_telemetry(
    ev_rx: &mut mpsc::Receiver<RunEvent>,
) -> (u32, Vec<EgressTag>, Vec<serde_json::Value>) {
    let mut escalations = 0u32;
    let mut egress = Vec::new();
    let mut per_stage = Vec::new();
    let mut attempt = 0u64;
    while let Ok(ev) = ev_rx.try_recv() {
        match ev {
            RunEvent::StageStarted { stage, tier, .. } => {
                let tier = tier.unwrap_or_else(|| "default".to_string());
                egress.push(EgressTag { attempt, tier: tier.clone(), egress: tier_egress(&tier) });
                per_stage.push(serde_json::json!({ "stage": stage, "attempt": attempt, "tier": tier }));
                attempt += 1;
            }
            RunEvent::Escalation { to, .. } => {
                escalations += 1;
                egress.push(EgressTag { attempt, tier: to.clone(), egress: tier_egress(&to) });
                per_stage.push(serde_json::json!({ "stage": "<escalated>", "attempt": attempt, "tier": to }));
                attempt += 1;
            }
            _ => {}
        }
    }
    (escalations, egress, per_stage)
}

/// Coarse tier→egress map (FMA-M2). The local llama ceiling is `medium`; `thinking`/`ultra`
/// route to cloud. `default` (no override) is treated as local. Documented as a heuristic until
/// a usage-returning backend surfaces the resolved backend per attempt.
fn tier_egress(tier: &str) -> String {
    match tier {
        "thinking" | "ultra" => "cloud",
        "fast" | "medium" | "default" => "local",
        _ => "unknown",
    }
    .to_string()
}

/// Persist an outcome to `eval_runs`, returning the new row id.
async fn persist(deps: &EvalDeps, outcome: &EvalOutcome) -> Result<String> {
    let per_stage_json = if outcome.per_stage_tokens.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(outcome.per_stage_tokens.clone()).to_string())
    };
    let egress_json = serde_json::to_string(&outcome.egress).ok();
    let gate_results_json = Some(serde_json::to_string(&outcome.score.gates).unwrap_or_default());
    let row = eval_runs::insert(
        &deps.db,
        NewEvalRun {
            task_id: &outcome.task_id,
            task_kind: "coding",
            model: &outcome.model,
            tier_config: &outcome.tier_config,
            depth: &outcome.depth,
            per_stage_tokens: per_stage_json.as_deref(),
            escalation_count: outcome.escalation_count as i64,
            egress: egress_json.as_deref(),
            wall_clock_ms: outcome.wall_clock_ms as i64,
            passed: outcome.score.passed,
            gate_results: gate_results_json.as_deref(),
        },
    )
    .await?;
    Ok(row.id)
}
