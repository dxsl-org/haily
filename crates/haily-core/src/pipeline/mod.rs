//! Pipeline engine — the Rust-native stage machine that converts a weak model into
//! HailyKit-class output by keeping sequencing, gating, and retry as deterministic code (the
//! LLM only fills bounded stages).
//!
//! This module (phase 4a) carries the DECLARATIVE types + pure helpers:
//! - [`stage`]: [`Stage`], [`Pipeline`], the goclaw-style [`StageOutcome`] exit codes, and
//!   [`RunStatus`] (typed mirror of `pipeline_runs.status`).
//! - [`gate`]: [`Gate`] (command / artifact / approval) + [`ArtifactKind`] parse helpers.
//! - [`verifier_output`]: the language-agnostic decisive-output parser for command gates.
//!
//! The RUNNER (sequential execution, retry loop, escalation, pause/resume, worktree
//! compensation, RunEvent emission, and the shared-handle threading that keeps the pipeline an
//! orthogonal orchestration axis rather than a delegation level) lands in phase 4b — it is the
//! agent-loop-touching half and is intentionally NOT in this PR.

pub mod automation_eval;
pub mod build_pipeline;
pub mod eval_runner;
pub mod exemplar;
pub mod gate;
pub mod judge;
pub mod launcher;
pub mod plan_pipeline;
pub mod runner;
pub mod stage;
pub mod verifier_output;

pub use automation_eval::{
    parse_automation_task, render_automation_outcome, run_automation_eval, AutomationDeps,
    AutomationOutcome, AutomationScore, AutomationTask,
};
pub use build_pipeline::{
    build_phase_pipeline, build_review_pipeline, run_build, ship_pipeline, BuildRunSpec,
    EmitFindingsTool, Finding, PhaseInput, Severity, VerifierCmd, EMIT_FINDINGS_TOOL,
};
pub use eval_runner::{
    parse_task_yaml, run_coding_eval, score, EvalDeps, EvalMode, EvalOutcome, ScoreInputs,
    ScoreResult, TaskManifest,
};
pub use gate::{ArtifactKind, Gate};
pub use launcher::{launch_coding_run, CodingRunSpec, LaunchDeps, RunKind};
pub use judge::{apex_judge, judge_panel, plan_design, refuter_votes, ApexVerdict, DesignResult, JudgeContext};
pub use plan_pipeline::{
    build_plan_pipeline, run_plan, EmitPlanDraftTool, PlanDraft, PlanRunSpec, RenderPlanTool,
};
pub use runner::{decide, PipelineRunner, RunReport, RunSpec, StageDecision};
pub use stage::{Pipeline, RunStatus, Stage, StageOutcome, DEFAULT_MAX_TOOL_CALLS};
pub use verifier_output::{parse_decisive, VerifierLang};
