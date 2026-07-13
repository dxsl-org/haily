//! Pipeline launcher (Pipeline Activation & Wiring, phase 1) — constructs a [`PipelineRunner`]
//! from the orchestrator's own handles and drives a live `run_plan`/`run_build` against a real
//! session, closing the "nothing constructs a runner in production" gap the eval harness
//! (`eval_runner/mod.rs:195-246`) was the only prior full example of.
//!
//! [`launch_coding_run`] is a free function over [`LaunchDeps`] (a plain struct of cloned
//! handles) rather than a method reaching into `Orchestrator`'s private fields from this sibling
//! module — mirrors `agent::TurnRuntime`'s own "copy handles out, then cross into the callee
//! module" convention. `Orchestrator::launch_coding_run` (in `lib.rs`) builds `LaunchDeps` from
//! `self.*` and delegates here.
//!
//! Target-repo resolution (explicit spec value → `coding.default_repo` preference → error) never
//! falls back to the process cwd for a write-tier run (Security Considerations, phase file). The
//! tool-registry/verifier-command helpers + per-kind spec builders live in [`registry`] to keep
//! this file under the project's 200-line guideline.

mod registry;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use anyhow::{bail, Result};
use haily_db::queries::{coding_workspaces, meta};
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::LlmRouter;
use haily_tools::coding::workspace::CodingWorkspace;
use haily_types::{ApprovalGate, DepthMode, Notification, ResponseChunk, RunEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::pipeline::build_pipeline::run_build;
use crate::pipeline::plan_pipeline::run_plan;
use crate::pipeline::runner::{PipelineRunner, RunReport};
use crate::pipeline::stage::RunStatus;
use registry::{base_registry, build_run_spec, plan_run_spec};

/// Preference key resolving the default target repo for a launch with no explicit `repo_path`
/// (the plan's "Target-repo resolution" open question — this phase owns the precedence).
const DEFAULT_REPO_PREF_KEY: &str = "coding.default_repo";

/// Persistent attempts budget seeded into each per-run liveness counter (FMA-C1). No fixture
/// manifest exists for a live launch to derive a task-specific ceiling from (unlike the eval
/// harness's `manifest.max_tool_calls`), so this mirrors the eval runner's own plan-run constant.
const DEFAULT_ATTEMPTS_BUDGET: i64 = 8;

/// Which stage(s) of the pipeline one launch drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    /// Scout→design→write→approval only.
    Plan,
    /// Build→verify→ship only. MVP: a single synthetic phase carrying the task text verbatim —
    /// no plan.md → `Vec<PhaseInput>` parser exists yet (plan.md's Open Question #2); a future
    /// phase can replace `registry::build_run_spec`'s phase shape without changing this enum.
    Build,
    /// Run Plan, then — only if it reaches [`RunStatus::Done`] — Build on the same task.
    PlanThenBuild,
}

/// One coding-pipeline launch request — the caller-facing input to
/// [`crate::Orchestrator::launch_coding_run`].
pub struct CodingRunSpec {
    pub kind: RunKind,
    pub task: String,
    pub session_id: Uuid,
    pub work_item_id: Option<String>,
    /// Explicit caller-supplied target repo (e.g. a GUI path picker or a slash-command arg).
    /// `None` falls back to the `coding.default_repo` preference; both absent is a resolution
    /// error — see [`resolve_repo_path`]. (Deviation from the phase file's literal `PathBuf`
    /// field type — see the phase's Deviation Log: the precedence the same file specifies
    /// requires distinguishing "no explicit value" from any real path.)
    pub repo_path: Option<PathBuf>,
    pub depth: DepthMode,
}

/// Handles a launch is constructed from, explicitly cloned out of `Orchestrator` by its caller
/// (mirrors `agent::TurnRuntime`) rather than this module reaching into private fields.
pub struct LaunchDeps {
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    pub llm: Arc<RwLock<Arc<LlmRouter>>>,
    /// The session's real approval broker, upcast to the trait object `PipelineRunner` expects —
    /// the SAME gate a normal turn uses, so every write-tier tool call inside a launched run
    /// stays gated (Security Considerations).
    pub broker: Arc<dyn ApprovalGate>,
    /// The session's real kill switch (`safety.disable_writes`) — honored identically to a
    /// normal turn.
    pub kill: Arc<AtomicBool>,
}

/// Drive `spec` to completion against a real target repo: resolve the repo, open an ephemeral
/// [`CodingWorkspace`], construct a [`PipelineRunner`] from `deps`, run Plan and/or Build, stamp
/// `coding_workspaces.run_id`, and (on a `Done` run) discard the now-spent workspace.
///
/// A normal (non-setup-failure) run always ends by sending [`ResponseChunk::Complete`] on
/// `user_tx` — mirrors `agent::run_turn`'s own terminal-Complete contract, so the app-layer
/// delivery task draining `user_tx` needs no special-casing for a pipeline-originated turn.
///
/// # Errors
/// Returns an error only for a setup failure (repo resolution, workspace open, or a runner
/// setup failure — see [`PipelineRunner::run`]'s own contract); a paused/failed pipeline run is
/// a normal [`RunReport`], not an error.
pub async fn launch_coding_run(
    deps: LaunchDeps,
    spec: CodingRunSpec,
    user_tx: mpsc::Sender<ResponseChunk>,
    events_tx: mpsc::Sender<RunEvent>,
    distillation_tx: Option<mpsc::Sender<Notification>>,
    cancel: CancellationToken,
) -> Result<RunReport> {
    let repo_path = resolve_repo_path(&deps.db, &spec).await?;
    let root = worktrees_root();
    let workspace = CodingWorkspace::open(
        &deps.db,
        &spec.session_id.to_string(),
        &repo_path,
        &root,
        spec.work_item_id.as_deref(),
    )
    .await?;

    let slug = spec
        .work_item_id
        .clone()
        .unwrap_or_else(|| spec.session_id.to_string());
    let base_tools = base_registry(&workspace, &slug, &spec.task);

    let runner = PipelineRunner::new(
        Arc::clone(&deps.db),
        Arc::clone(&deps.kms),
        Arc::clone(&deps.llm),
        base_tools,
        Arc::clone(&deps.broker),
        Arc::clone(&deps.kill),
        cancel,
        user_tx.clone(),
        events_tx,
        // The runner's own documented default (`escalation_enabled` = P3 policy, off by
        // default). No live preference gates this yet for a launched run — a follow-up can
        // thread `llm.escalation.enabled` here without changing this function's signature.
        false,
    );

    let report = match spec.kind {
        RunKind::Plan => {
            run_plan(
                &runner,
                &deps.db,
                plan_run_spec(&spec, &slug, &workspace, None, DEFAULT_ATTEMPTS_BUDGET),
            )
            .await?
        }
        RunKind::Build => {
            run_build(
                &runner,
                &deps.db,
                build_run_spec(&spec, &workspace, distillation_tx, DEFAULT_ATTEMPTS_BUDGET),
            )
            .await?
        }
        RunKind::PlanThenBuild => {
            let plan_report = run_plan(
                &runner,
                &deps.db,
                plan_run_spec(&spec, &slug, &workspace, None, DEFAULT_ATTEMPTS_BUDGET),
            )
            .await?;
            if plan_report.status == RunStatus::Done {
                run_build(
                    &runner,
                    &deps.db,
                    build_run_spec(&spec, &workspace, distillation_tx, DEFAULT_ATTEMPTS_BUDGET),
                )
                .await?
            } else {
                plan_report
            }
        }
    };

    finalize_workspace(&deps.db, workspace, &report).await;
    let _ = user_tx.send(ResponseChunk::Complete).await;
    Ok(report)
}

/// Resolve the target repo: explicit spec value, else `coding.default_repo`, else an error.
/// Never falls back to the process cwd for a write-tier run.
async fn resolve_repo_path(db: &DbHandle, spec: &CodingRunSpec) -> Result<PathBuf> {
    if let Some(p) = &spec.repo_path {
        return Ok(p.clone());
    }
    match meta::get_preference(db, DEFAULT_REPO_PREF_KEY).await? {
        Some(v) if !v.trim().is_empty() => Ok(PathBuf::from(v)),
        _ => bail!(
            "no target repo resolved: pass an explicit repo path or set the \
             '{DEFAULT_REPO_PREF_KEY}' preference (a write-tier coding run never defaults to \
             the process cwd)"
        ),
    }
}

/// Fixed, discoverable base dir for ephemeral per-run worktrees — NOT a self-deleting
/// `tempfile::tempdir()` like the throwaway eval harness, so the `coding_workspaces` row's
/// `worktree_path` stays valid on disk for the P6 reaper to find and reconcile after this
/// process exits.
fn worktrees_root() -> PathBuf {
    std::env::temp_dir().join("haily-coding-worktrees")
}

/// Stamp `coding_workspaces.run_id` and, on a `Done` run, discard the now-spent workspace.
/// Any other terminal status (Paused/Failed/Interrupted) keeps the workspace so a follow-up
/// trigger or the P6 reaper can inspect or resume it. Best-effort: a stamp/discard failure is
/// logged, never propagated — it must not turn a completed pipeline run into a reported error.
async fn finalize_workspace(db: &DbHandle, workspace: CodingWorkspace, report: &RunReport) {
    if let Err(e) = coding_workspaces::set_run_id(db, &workspace.row.id, &report.run_id).await {
        tracing::warn!(
            workspace = %workspace.row.id,
            run_id = %report.run_id,
            "coding_workspaces run_id stamp failed: {e:#}"
        );
    }
    if report.status == RunStatus::Done {
        if let Err(e) = workspace.discard(db).await {
            tracing::warn!(workspace = %workspace.row.id, "workspace discard after a Done run failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests;
