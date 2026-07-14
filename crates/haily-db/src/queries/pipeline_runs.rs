//! Pipeline-run persistence — the resumable state of the P4 stage machine.
//!
//! Mirrors the `work_items` lifecycle idiom (FromRow struct, RFC3339 timestamps,
//! `deleted_at IS NULL` guards, all SQL kept in this crate). The runner (P4b) drives
//! transitions; `attempts_remaining` is the authoritative pipeline-global liveness bound
//! (red-team FMA-C1), decremented across attempts and surviving restart.
use crate::DbHandle;
use anyhow::Result;
use sqlx::{Executor, FromRow, Sqlite, Transaction};
use uuid::Uuid;

/// One pipeline run. `status` is a [`crate::queries::pipeline_runs`]-agnostic string
/// (`queued`/`running`/`paused`/`interrupted`/`done`/`failed`) — the typed `RunStatus`
/// enum lives in `haily-core::pipeline`, which this leaf crate must not depend on, so the
/// mapping is owned there.
#[derive(Debug, Clone, FromRow)]
pub struct PipelineRun {
    pub id: String,
    pub work_item_id: Option<String>,
    pub session_id: String,
    pub stage_index: i64,
    pub status: String,
    pub attempt: i64,
    pub attempts_remaining: i64,
    pub tier_used: Option<String>,
    pub backend_used: Option<String>,
    pub egress: Option<String>,
    pub gate_output_digest: Option<String>,
    /// P6 forward column (nullable) — synthesized findings; unused until P6.
    pub findings: Option<String>,
    /// P8 forward column (nullable JSON) — per-attempt token accounting; unused until P8.
    pub per_attempt_tokens: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

/// Every mutable field a stage transition can advance, grouped into one struct so
/// [`transition`] stays within a sane arity (the `journal::NewAction` idiom) and the
/// runner's call-site reads as a single literal at the transition point.
pub struct RunTransition<'a> {
    pub stage_index: i64,
    /// New run status (see [`PipelineRun::status`]).
    pub status: &'a str,
    pub attempt: i64,
    /// Persistent liveness bound after this transition (FMA-C1).
    pub attempts_remaining: i64,
    pub tier_used: Option<&'a str>,
    pub backend_used: Option<&'a str>,
    pub egress: Option<&'a str>,
    pub gate_output_digest: Option<&'a str>,
}

/// Create a new run in `queued` state at stage 0.
///
/// `attempts_remaining` seeds the persistent liveness counter (FMA-C1) — the pipeline-global
/// bound the runner decrements across attempts, NOT the per-turn LoopGuard.
///
/// # Errors
/// Returns an error if `session_id`/`work_item_id` do not reference valid rows or the insert
/// fails.
pub async fn create(
    db: &DbHandle,
    session_id: &str,
    work_item_id: Option<&str>,
    attempts_remaining: i64,
) -> Result<PipelineRun> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, PipelineRun>(
        "INSERT INTO pipeline_runs
             (id, work_item_id, session_id, stage_index, status, attempt,
              attempts_remaining, created_at, updated_at)
         VALUES (?, ?, ?, 0, 'queued', 0, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(work_item_id)
    .bind(session_id)
    .bind(attempts_remaining)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// Shared body for [`transition`]/[`transition_tx`] — generic over any sqlx `Executor` so the
/// pool- and transaction-scoped callers share one copy of the SQL. The runner (P4b) writes a
/// stage transition INSIDE the same transaction as the matching `action_journal` row so a
/// crash between them is impossible (red-team FMA-C2) — that is why the tx-scoped variant
/// exists here rather than only a pool-scoped one.
async fn transition_via<'e, E>(exec: E, id: &str, t: RunTransition<'_>) -> Result<bool>
where
    E: Executor<'e, Database = Sqlite>,
{
    let now = chrono::Utc::now().to_rfc3339();
    let result = sqlx::query(
        "UPDATE pipeline_runs
         SET stage_index = ?, status = ?, attempt = ?, attempts_remaining = ?,
             tier_used = ?, backend_used = ?, egress = ?, gate_output_digest = ?,
             updated_at = ?
         WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(t.stage_index)
    .bind(t.status)
    .bind(t.attempt)
    .bind(t.attempts_remaining)
    .bind(t.tier_used)
    .bind(t.backend_used)
    .bind(t.egress)
    .bind(t.gate_output_digest)
    .bind(&now)
    .bind(id)
    .execute(exec)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Advance a run's stage state in a single UPDATE. Returns `true` if an active row was
/// updated, `false` if none matched `id` (already terminal-deleted, cancelled, or never
/// existed) — the runner (P4b) uses this to detect a concurrently-cancelled run and stop
/// driving it rather than looping on a vanished row (review MED).
///
/// # Errors
/// Returns an error if the update fails.
pub async fn transition(db: &DbHandle, id: &str, t: RunTransition<'_>) -> Result<bool> {
    transition_via(db.pool(), id, t).await
}

/// Transaction-scoped [`transition`] — used by the runner so the run transition commits
/// atomically with its paired `action_journal` write (FMA-C2). Returns `true` iff an active
/// row was updated (see [`transition`]).
///
/// # Errors
/// Returns an error if the update fails.
pub async fn transition_tx(
    tx: &mut Transaction<'_, Sqlite>,
    id: &str,
    t: RunTransition<'_>,
) -> Result<bool> {
    transition_via(&mut **tx, id, t).await
}

/// Commit a run's terminal/pause transition AND its audit-marker journal row in ONE
/// transaction (red-team FMA-C2). The runner's cancel-proof finalize calls this so a crash or
/// kill between the two writes is impossible: either the run advanced AND the journal recorded
/// it, or neither did. `marker` is the run-level audit row (a `pipeline_run` marker with no
/// `compensation_plan` — the worktree, not this row, is the compensator); it is deliberately
/// NOT stamped with `run_id`, so `undo_run` never tries to compensate the marker itself.
///
/// Returns the transition's `rows_affected > 0` (see [`transition`]) — `false` means the run
/// row vanished (cancelled/deleted) before finalize, in which case the marker still committed
/// as an audit trail.
///
/// # Errors
/// Returns an error if beginning the transaction, either write, or the commit fails.
pub async fn finalize(
    db: &DbHandle,
    id: &str,
    t: RunTransition<'_>,
    marker: crate::queries::journal::NewAction<'_>,
) -> Result<bool> {
    let mut tx = db.pool().begin().await?;
    crate::queries::journal::insert_tx(&mut tx, marker).await?;
    let advanced = transition_via(&mut *tx, id, t).await?;
    tx.commit().await?;
    Ok(advanced)
}

/// Persist synthesized review findings onto a run's pre-allocated nullable `findings` column
/// (Sub-Agent + Skill Architecture P6). The column exists since P4a (a forward slot), so this
/// is an in-place UPDATE — no migration. `findings_json` is the caller-serialized findings
/// array (already validated + tag-stripped upstream); this layer stores it verbatim.
///
/// Returns `true` iff an active row was updated (`false` = vanished/soft-deleted — the caller
/// treats it as a best-effort write, exactly like [`transition`]).
///
/// # Errors
/// Returns an error if the update fails.
pub async fn set_findings(db: &DbHandle, id: &str, findings_json: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE pipeline_runs SET findings = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(findings_json)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// Persist per-ATTEMPT token accounting onto a run's pre-allocated nullable `per_attempt_tokens`
/// column (Sub-Agent + Skill Architecture phase 8, FMA-m5). The column exists since P4a (a
/// forward slot), so this is an in-place UPDATE — no migration. `tokens_json` is the
/// caller-serialized array of per-attempt records (each with its resolved backend + paired
/// usage); this layer stores it verbatim. Returns `true` iff an active row was updated.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn set_per_attempt_tokens(db: &DbHandle, id: &str, tokens_json: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE pipeline_runs SET per_attempt_tokens = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(tokens_json)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// Reset any run left `running` or `queued` by a crash/kill to `interrupted` — the pipeline
/// analogue of `work_items::reset_stale_running`, run once at boot BEFORE any resume is offered
/// (FMA-m4: an interrupted run's write stages never auto-resume; the user resumes explicitly).
/// A `paused` run is already user-visible and is left as-is. Returns the number reset.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn reset_stale_running(db: &DbHandle) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE pipeline_runs SET status = 'interrupted', updated_at = ?
         WHERE status IN ('running', 'queued') AND deleted_at IS NULL",
    )
    .bind(&now)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows)
}

/// Get a single non-deleted run by id. `None` if no active run with that id exists.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get(db: &DbHandle, id: &str) -> Result<Option<PipelineRun>> {
    Ok(sqlx::query_as::<_, PipelineRun>(
        "SELECT * FROM pipeline_runs WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await?)
}

/// List all non-terminal, non-deleted runs (queued/running/paused/interrupted), oldest first.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_active(db: &DbHandle) -> Result<Vec<PipelineRun>> {
    Ok(sqlx::query_as::<_, PipelineRun>(
        "SELECT * FROM pipeline_runs
         WHERE status IN ('queued', 'running', 'paused', 'interrupted')
           AND deleted_at IS NULL
         ORDER BY created_at ASC",
    )
    .fetch_all(db.pool())
    .await?)
}

/// List only `interrupted`, non-deleted runs — surfaced to the user on boot for explicit
/// resume (never auto-resumed; FMA-m4).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_interrupted(db: &DbHandle) -> Result<Vec<PipelineRun>> {
    Ok(sqlx::query_as::<_, PipelineRun>(
        "SELECT * FROM pipeline_runs
         WHERE status = 'interrupted' AND deleted_at IS NULL
         ORDER BY created_at ASC",
    )
    .fetch_all(db.pool())
    .await?)
}

/// Count all non-deleted runs for a session, ANY status (queued through done/failed) — the
/// journal-completeness signal the P9 coding eval scores on (a completed run is `done`, which
/// [`list_active`] deliberately excludes, so the eval needs an any-status count).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn count_for_session(db: &DbHandle, session_id: &str) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pipeline_runs WHERE session_id = ? AND deleted_at IS NULL",
    )
    .bind(session_id)
    .fetch_one(db.pool())
    .await?;
    Ok(row.0)
}

/// Fetch just the `status` string of one run (the P6 worktree reaper's terminal-run check) —
/// avoids pulling the whole row when only the status is needed. `None` if `id` does not
/// reference an active run (never existed, or soft-deleted); the caller treats that identically
/// to "unknown", never as a reap signal on its own — see the `haily-app` reaper's `is_reapable`.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn status_of(db: &DbHandle, id: &str) -> Result<Option<String>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT status FROM pipeline_runs WHERE id = ? AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(db.pool())
            .await?;
    Ok(row.map(|(status,)| status))
}

/// Soft-delete a run. C10-guarded (`WHERE id = ? AND deleted_at IS NULL`) so a double-delete
/// is detected via `rows_affected()` rather than a separate SELECT.
///
/// Returns `true` if a row was actually deleted, `false` if `id` did not match an active row.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn soft_delete(db: &DbHandle, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE pipeline_runs SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}
