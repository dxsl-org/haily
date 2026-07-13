//! Per-launch tool registry, verifier-command resolution, and stage-spec builders — split out
//! of `launcher/mod.rs` to keep it under the project's 200-line guideline. Pure helpers only;
//! no orchestration lives here.

use std::path::Path;

use haily_tools::coding::stack_detect::{detect_stacks, Stack};
use haily_tools::coding::workspace::CodingWorkspace;
use haily_tools::ToolRegistry;
use haily_types::Notification;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::pipeline::build_pipeline::{BuildRunSpec, EmitFindingsTool, PhaseInput, VerifierCmd};
use crate::pipeline::plan_pipeline::{EmitPlanDraftTool, PlanRunSpec, RenderPlanTool};

use super::CodingRunSpec;

/// Build the LIVE base registry a run's stages snapshot their per-stage whitelist from: the
/// FULL V1 coding tool surface (including `worktree_apply`/`git_commit` — unlike the eval
/// harness's structurally ship-blocked registry, a live run is MEANT to reach the real target
/// repo) plus the three synthetic per-run pipeline tools bound to this workspace.
pub(super) fn base_registry(
    workspace: &CodingWorkspace,
    slug: &str,
    task: &str,
) -> Arc<ToolRegistry> {
    let mut reg = ToolRegistry::build_v1();
    let root = workspace.worktree_root().to_path_buf();
    reg.register(Arc::new(EmitPlanDraftTool::new(root.clone(), slug)));
    reg.register(Arc::new(RenderPlanTool::new(root, slug, task)));
    reg.register(Arc::new(EmitFindingsTool));
    Arc::new(reg)
}

/// Resolve the compile/test verifier commands for a workspace's detected stack (reuses P2
/// `stack_detect`; first detected stack wins, same stable order `VerifierLang::detect` uses). An
/// unrecognized stack falls back to a trivially-passing `git --version` gate rather than
/// crashing the launch — `git` is already a hard runtime dependency of every `CodingWorkspace`,
/// so it always resolves; a recognized stack whose command happens to be absent on the host
/// still degrades gracefully via the runner's own `VerifierAbsent` path (AD-M3) rather than
/// aborting the run.
fn verifier_commands(root: &Path) -> (VerifierCmd, VerifierCmd) {
    match detect_stacks(root).first() {
        Some(Stack::Rust) => (
            VerifierCmd::new("cargo", &["check", "--quiet"]),
            VerifierCmd::new("cargo", &["test", "--quiet"]),
        ),
        Some(Stack::TypeScript) => (
            VerifierCmd::new("npm", &["run", "build", "--silent"]),
            VerifierCmd::new("npm", &["test", "--silent"]),
        ),
        Some(Stack::Python) => (
            VerifierCmd::new("python", &["-m", "compileall", "-q", "."]),
            VerifierCmd::new("pytest", &["-q"]),
        ),
        Some(Stack::Go) => (
            VerifierCmd::new("go", &["build", "./..."]),
            VerifierCmd::new("go", &["test", "./..."]),
        ),
        Some(Stack::Java) => (
            VerifierCmd::new("mvn", &["-q", "compile"]),
            VerifierCmd::new("mvn", &["-q", "test"]),
        ),
        None => (
            VerifierCmd::new("git", &["--version"]),
            VerifierCmd::new("git", &["--version"]),
        ),
    }
}

/// Build a [`PlanRunSpec`] from `spec`, borrowing `workspace` for its lifetime.
pub(super) fn plan_run_spec<'a>(
    spec: &CodingRunSpec,
    slug: &str,
    workspace: &'a CodingWorkspace,
    revise_feedback: Option<String>,
    attempts_budget: i64,
) -> PlanRunSpec<'a> {
    PlanRunSpec {
        task: spec.task.clone(),
        slug: slug.to_string(),
        session_id: spec.session_id,
        work_item_id: spec.work_item_id.clone(),
        attempts_budget,
        workspace,
        revise_feedback,
        depth: spec.depth,
    }
}

/// Build a [`BuildRunSpec`] from `spec` — the MVP single-synthetic-phase shape (see
/// [`super::RunKind::Build`]'s doc).
pub(super) fn build_run_spec<'a>(
    spec: &CodingRunSpec,
    workspace: &'a CodingWorkspace,
    distillation_tx: Option<mpsc::Sender<Notification>>,
    attempts_budget: i64,
) -> BuildRunSpec<'a> {
    let (compile, test) = verifier_commands(workspace.worktree_root());
    let phase = PhaseInput {
        name: "impl".to_string(),
        tier: None,
        content: spec.task.clone(),
        target_files: Vec::new(),
    };
    BuildRunSpec {
        phases: vec![phase],
        session_id: spec.session_id,
        work_item_id: spec.work_item_id.clone(),
        attempts_budget,
        workspace,
        compile,
        test,
        depth: spec.depth,
        distillation_tx,
    }
}
