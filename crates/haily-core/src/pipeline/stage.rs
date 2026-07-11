//! Stage / Pipeline definition types + the goclaw-style exit-code stage model.
//!
//! These are the DECLARATIVE description of a pipeline. The runner that executes them is
//! P4b — this module carries the types and their pure helpers only. Keeping sequencing,
//! gating, and retry as deterministic Rust (the LLM only fills bounded stages) is the whole
//! weak-model strategy: the code is what turns a "Sonnet-class model" into "HailyKit-class
//! output".

use super::gate::Gate;
use haily_llm::Tier;

/// The delegation-tool name prefix a stage whitelist must NEVER contain. Stages are LEAVES
/// (red-team AD-C1/DEP-C1): the runner is the sole orchestrator, so a stage sub-turn must not
/// be able to call `delegate_to_X` — otherwise runner-nesting × delegation-nesting could blow
/// past the depth-2 cap. Enforced by [`Stage::whitelist_excludes_delegation`].
pub const DELEGATION_TOOL_PREFIX: &str = "delegate_to";

/// One bounded unit of a pipeline: a single `run_sub_turn` invocation the runner drives with
/// an overridden tool budget and a leaf-only whitelist, gated by [`Gate`].
#[derive(Debug, Clone)]
pub struct Stage {
    /// Human/log-facing stage name (e.g. `"plan"`, `"implement"`, `"verify"`).
    pub name: String,
    /// Model tier this stage runs on. `None` = inherit the run/session default (no override);
    /// the runner resolves it. Escalation (P3 policy) may raise it on retry.
    pub tier: Option<Tier>,
    /// Name of the AUTHORED stage-prompt (a curated prompt in the kit-pack), NOT inline prompt
    /// text — the runner loads it and appends dynamic context. Keeping it a reference keeps
    /// prompt content out of code and versionable.
    pub prompt_ref: String,
    /// Tools this stage may call. MUST exclude every `delegate_to_*` (stages are leaves) — see
    /// [`Stage::whitelist_excludes_delegation`].
    pub tool_whitelist: Vec<String>,
    /// Per-stage tool-call budget — replaces the chat-scale limit. Default ≤25 (P0 spike data,
    /// below the ~25–30 coherence ceiling; red-team AD-C2/FMA-M1). LoopGuard semantics stay
    /// terminate-not-feed-back; this is a bound, not a new loop.
    pub max_tool_calls: u32,
    /// The gate that decides whether this stage passed.
    pub gate: Gate,
    /// Per-stage verifier-grounded retry budget. On gate failure the decisive output is fed
    /// back into the SAME stage; after `max_retries` the runner escalates (P3) then pauses.
    /// The authoritative pipeline-GLOBAL bound is `pipeline_runs.attempts_remaining`, not this.
    pub max_retries: u32,
    /// Optional GBNF grammar forcing this stage's generation shape (Plan Pipeline, P5). `Some`
    /// only for a stage that constrains its output to a specific tool-call JSON (the Design
    /// stage's `emit_plan_draft`); `None` leaves generation unconstrained. llama-only — the
    /// cloud path ignores it, so parse-and-repair stays the primary path off-llama. The runner
    /// copies this onto the stage sub-turn's `SubTurnRequest`.
    pub grammar: Option<String>,
}

/// Default per-stage tool-call budget (red-team AD-C2/FMA-M1: below the ~25–30 coherence
/// ceiling the research cites, and deliberately NOT the original 40).
pub const DEFAULT_MAX_TOOL_CALLS: u32 = 25;

impl Stage {
    /// True iff no whitelisted tool is a delegation tool (name starts with
    /// [`DELEGATION_TOOL_PREFIX`]). The runner MUST reject a stage that fails this (AD-C1):
    /// stages are leaves, the runner is the sole orchestration axis.
    pub fn whitelist_excludes_delegation(&self) -> bool {
        !self
            .tool_whitelist
            .iter()
            .any(|t| t.starts_with(DELEGATION_TOOL_PREFIX))
    }
}

/// An ordered pipeline of stages. The runner executes `runs` sequentially.
#[derive(Debug, Clone)]
pub struct Pipeline {
    /// Ordered stages. Field name matches the plan's `Pipeline { runs: Vec<Stage> }`.
    pub runs: Vec<Stage>,
}

impl Pipeline {
    /// True iff EVERY stage's whitelist excludes delegation tools (AD-C1). The runner should
    /// validate this before executing so a malformed pipeline never reaches a stage sub-turn.
    pub fn all_stages_are_leaves(&self) -> bool {
        self.runs.iter().all(Stage::whitelist_excludes_delegation)
    }
}

/// goclaw-style exit-code stage control (reference: goclaw `internal/pipeline/pipeline.go`).
/// Each stage evaluation returns one of these so the runner's retry/escalation/pause edges
/// are an explicit, table-testable code path rather than ad-hoc prose branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    /// Stage passed — advance to the next stage.
    Continue,
    /// Task is done — exit the outer loop now (no further stages).
    BreakLoop,
    /// Unrecoverable / over-budget — break now and mark the run failed/paused.
    AbortRun,
}

/// Persisted lifecycle state of a run — the typed mirror of `pipeline_runs.status`.
///
/// Ordering is NOT semantic; this is a closed set of string-backed states. `as_str`/`parse`
/// own the on-the-wire mapping so the DB layer (which must not depend on `haily-core`) stays a
/// plain `TEXT` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Queued,
    Running,
    Paused,
    Interrupted,
    Done,
    Failed,
}

impl RunStatus {
    /// The canonical DB string for this status.
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::Paused => "paused",
            RunStatus::Interrupted => "interrupted",
            RunStatus::Done => "done",
            RunStatus::Failed => "failed",
        }
    }

    /// True for a terminal state (`done`/`failed`) that never resumes.
    pub fn is_terminal(self) -> bool {
        matches!(self, RunStatus::Done | RunStatus::Failed)
    }

    /// Parse a DB status string. `None` for an unrecognized value (fail-closed: the caller
    /// treats an unknown status as needing manual attention rather than guessing a state).
    pub fn parse(s: &str) -> Option<RunStatus> {
        Some(match s {
            "queued" => RunStatus::Queued,
            "running" => RunStatus::Running,
            "paused" => RunStatus::Paused,
            "interrupted" => RunStatus::Interrupted,
            "done" => RunStatus::Done,
            "failed" => RunStatus::Failed,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::gate::Gate;

    fn stage_with(whitelist: &[&str]) -> Stage {
        Stage {
            name: "s".into(),
            tier: None,
            prompt_ref: "p".into(),
            tool_whitelist: whitelist.iter().map(|s| s.to_string()).collect(),
            max_tool_calls: DEFAULT_MAX_TOOL_CALLS,
            gate: Gate::Approval { prompt: "ok?".into() },
            max_retries: 1,
            grammar: None,
        }
    }

    #[test]
    fn whitelist_without_delegation_is_a_leaf() {
        let s = stage_with(&["fs_read", "fs_edit", "shell_exec"]);
        assert!(s.whitelist_excludes_delegation());
    }

    #[test]
    fn whitelist_with_delegation_tool_is_not_a_leaf() {
        // AD-C1 regression: any delegate_to_* in a stage whitelist must be detectable so the
        // runner can reject the pipeline before a stage sub-turn ever runs.
        let s = stage_with(&["fs_read", "delegate_to_developer"]);
        assert!(!s.whitelist_excludes_delegation());
    }

    #[test]
    fn pipeline_leaf_check_spans_all_stages() {
        let good = Pipeline { runs: vec![stage_with(&["fs_read"]), stage_with(&["shell_exec"])] };
        assert!(good.all_stages_are_leaves());
        let bad = Pipeline {
            runs: vec![stage_with(&["fs_read"]), stage_with(&["delegate_to_researcher"])],
        };
        assert!(!bad.all_stages_are_leaves());
    }

    #[test]
    fn run_status_roundtrips_through_db_string() {
        for st in [
            RunStatus::Queued,
            RunStatus::Running,
            RunStatus::Paused,
            RunStatus::Interrupted,
            RunStatus::Done,
            RunStatus::Failed,
        ] {
            assert_eq!(RunStatus::parse(st.as_str()), Some(st));
        }
        assert_eq!(RunStatus::parse("nonsense"), None);
    }

    #[test]
    fn only_done_and_failed_are_terminal() {
        assert!(RunStatus::Done.is_terminal());
        assert!(RunStatus::Failed.is_terminal());
        assert!(!RunStatus::Paused.is_terminal());
        assert!(!RunStatus::Interrupted.is_terminal());
        assert!(!RunStatus::Running.is_terminal());
    }
}
