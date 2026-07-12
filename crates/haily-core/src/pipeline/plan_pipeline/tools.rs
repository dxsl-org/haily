//! The two synthetic stage tools that carry the Plan Pipeline's deterministic Rust logic:
//! [`EmitPlanDraftTool`] (Design stage) persists the forced-JSON draft, and [`RenderPlanTool`]
//! (Write stage) renders it into the plan artifacts.
//!
//! Both are `ReversibleWrite` and write DIRECTLY into the run's worktree (bound at
//! construction). They do NOT emit a separate journal row: the worktree is the single
//! authoritative compensator for in-workspace writes (red-team FMA-C2 — "two compensators over
//! the same bytes is a bug"), and the runner reverts these files via `compensate()` on retry or
//! failure exactly like any other workspace change. They resolve the worktree from a captured
//! path rather than an LLM-supplied `workspace_id`, so a weak model never has to know the id.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use haily_tools::{RiskTier, Tool, ToolContext};
use serde_json::{json, Value};

use super::draft::{draft_from_args, parse_and_repair, plan_draft_schema, EMIT_PLAN_DRAFT_TOOL};
use super::render::plan_artifacts;

/// The Write stage's synthetic render tool name.
pub const RENDER_PLAN_TOOL: &str = "render_plan";

/// Workspace-relative path of the persisted draft (under the plan dir's `reports/`).
fn draft_rel(slug: &str) -> String {
    format!(".agents/{slug}/reports/plan-draft.json")
}

/// Design-stage tool: accepts the plan draft as JSON args (grammar-forced or parse-repaired)
/// and persists the canonical form for the Write stage to render. Its `parameters_schema` is
/// the `PlanDraft` schema, which is also what the stage's GBNF grammar is built from.
pub struct EmitPlanDraftTool {
    worktree_root: PathBuf,
    slug: String,
}

impl EmitPlanDraftTool {
    pub fn new(worktree_root: impl Into<PathBuf>, slug: impl Into<String>) -> Self {
        Self { worktree_root: worktree_root.into(), slug: slug.into() }
    }
}

#[async_trait]
impl Tool for EmitPlanDraftTool {
    fn name(&self) -> &str {
        EMIT_PLAN_DRAFT_TOOL
    }
    fn description(&self) -> &str {
        "Record the structured plan draft (approach, rejected alternatives, phases, assumptions) as JSON."
    }
    fn parameters_schema(&self) -> Value {
        plan_draft_schema()
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        // A malformed draft returns an Err here — surfaced to the stage model as the tool
        // result so its next attempt can correct it, and (since no file is written) the
        // Design stage's `Gate::Artifact` fails, driving the runner's verifier-grounded retry.
        let draft = draft_from_args(&args)?;
        let path = self.worktree_root.join(draft_rel(&self.slug));
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.context("creating plan draft dir")?;
        }
        let canonical = serde_json::to_string_pretty(&draft).context("serializing plan draft")?;
        tokio::fs::write(&path, canonical).await.context("writing plan draft")?;
        Ok(format!(
            "plan draft recorded: {} phase(s), {} rejected alternative(s)",
            draft.phases.len(),
            draft.rejected.len()
        ))
    }
}

/// Write-stage tool: reads the persisted draft and renders it (deterministically) into
/// `plan.md` + `phase-NN-*.md` + `reports/scout-report.md` in the worktree. Takes no args — the
/// harness, not the model, owns the byte-level rendering.
pub struct RenderPlanTool {
    worktree_root: PathBuf,
    slug: String,
    task: String,
}

impl RenderPlanTool {
    pub fn new(
        worktree_root: impl Into<PathBuf>,
        slug: impl Into<String>,
        task: impl Into<String>,
    ) -> Self {
        Self { worktree_root: worktree_root.into(), slug: slug.into(), task: task.into() }
    }
}

#[async_trait]
impl Tool for RenderPlanTool {
    fn name(&self) -> &str {
        RENDER_PLAN_TOOL
    }
    fn description(&self) -> &str {
        "Render the recorded plan draft into plan.md and per-phase files in the workspace."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }
    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<String> {
        let draft_path = self.worktree_root.join(draft_rel(&self.slug));
        let raw = tokio::fs::read_to_string(&draft_path)
            .await
            .with_context(|| format!("reading plan draft {}", draft_path.display()))?;
        let draft = parse_and_repair(&raw)?;
        let artifacts = plan_artifacts(&self.task, &self.slug, &draft);
        for (rel, content) in &artifacts {
            let path = self.worktree_root.join(rel);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.context("creating plan artifact dir")?;
            }
            tokio::fs::write(&path, content).await.context("writing plan artifact")?;
        }
        Ok(format!("rendered {} plan artifact(s)", artifacts.len()))
    }
}
