//! Runs-screen read surface (Unified Chat UI phase 7, D6). Pure delegation onto
//! `haily-db::queries::pipeline_runs`, mirroring `cockpit.rs`'s `WorkspaceView`/`list_workspaces`
//! convention: every DB read and every DTO shape lives here, `src-tauri` stays glue-only.
use crate::run_control::is_resumable;
use haily_db::queries::pipeline_runs::{self, PipelineRun};
use haily_db::DbHandle;
use serde::Serialize;

/// Bounded window of terminal (done/failed) history alongside every active run — the Key
/// Insight/Risk Assessment call: "load more" pagination is deferred (YAGNI), a fixed cap is
/// enough for the phase's success criteria.
const RECENT_TERMINAL_LIMIT: i64 = 50;

/// One run row for the Runs screen. Deliberately carries RAW status/reason fields rather than a
/// pre-rendered sentence — VN narration is derived client-side by `run-narration.ts` (the single
/// place that phrase is authored, per its own module doc), overlaid with the live reducer's
/// richer per-event narration for an in-flight run. `resumable` mirrors `WorkspaceView`'s own
/// contract: computed HERE from the SAME guard `resume_run` enforces, never re-derived in the
/// frontend, so the enable rule can never drift (the exact drift class the plan's red-team
/// flagged for `WorkspaceView`).
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub id: String,
    pub session_id: String,
    pub work_item_id: Option<String>,
    pub status: String,
    pub pause_reason_class: Option<String>,
    /// The originating task text (`pipeline_runs.task`), `None` for a pre-migration/eval row.
    pub task: Option<String>,
    pub stage_index: i64,
    pub attempt: i64,
    pub attempts_remaining: i64,
    pub tier_used: Option<String>,
    pub backend_used: Option<String>,
    /// Raw per-attempt token JSON array (FMA-m5) — `None` until at least one attempt recorded
    /// one. Parsed client-side by `RunTelemetry.svelte`; passed through verbatim here.
    pub per_attempt_tokens: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Whether "Tiếp tục" (`resumeRun`) should be offered — see [`is_resumable`]'s doc.
    pub resumable: bool,
}

impl From<PipelineRun> for RunSummary {
    fn from(row: PipelineRun) -> Self {
        let resumable = is_resumable(&row.status, row.pause_reason_class.as_deref());
        Self {
            id: row.id,
            session_id: row.session_id,
            work_item_id: row.work_item_id,
            status: row.status,
            pause_reason_class: row.pause_reason_class,
            task: row.task,
            stage_index: row.stage_index,
            attempt: row.attempt,
            attempts_remaining: row.attempts_remaining,
            tier_used: row.tier_used,
            backend_used: row.backend_used,
            per_attempt_tokens: row.per_attempt_tokens,
            created_at: row.created_at,
            updated_at: row.updated_at,
            resumable,
        }
    }
}

/// List runs for the Runs screen: every active row plus a bounded window of recent terminal
/// history — see [`pipeline_runs::list_runs`]'s doc for the exact query contract.
///
/// # Errors
/// Returns an error only if the underlying query fails.
pub async fn list_runs(db: &DbHandle) -> anyhow::Result<Vec<RunSummary>> {
    Ok(pipeline_runs::list_runs(db, RECENT_TERMINAL_LIMIT)
        .await?
        .into_iter()
        .map(RunSummary::from)
        .collect())
}
