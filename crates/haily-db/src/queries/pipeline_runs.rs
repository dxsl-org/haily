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
    /// Unified Chat UI phase 6 (D3) forward column — the original `CodingRunSpec.task` this
    /// row's launch was driving, so `resume_run` can reconstruct a relaunch. `None` for a row
    /// with no resume context (pre-migration, eval/test runs).
    pub task: Option<String>,
    /// Unified Chat UI phase 6 (D3) — `"plan"` or `"build"` (never `"plan_then_build"`; a
    /// composite launch re-stamps this per portion — see `PipelineRunner::seed_launch`).
    pub run_kind: Option<String>,
    /// Unified Chat UI phase 6 (D3) — the originating `DepthMode` label (`quick`/`normal`/`deep`).
    pub depth: Option<String>,
    /// Unified Chat UI phase 6 (D3) — set only when `status = "paused"`; see
    /// `haily-core::pipeline::runner::PauseReasonClass`. `resume_run` reads ONLY this column,
    /// never the free-text `RunEvent::RunPaused.reason` string.
    pub pause_reason_class: Option<String>,
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
    /// Unified Chat UI phase 6 (D3) — set (via [`PauseReasonClass::as_str`]) only on the
    /// transition that pauses a run; every other transition passes `None`, which clears any
    /// stale class left over from an earlier pause on a since-resumed row.
    pub pause_reason_class: Option<&'a str>,
}

/// The original `CodingRunSpec` context needed to relaunch a paused/interrupted row (Unified
/// Chat UI phase 6, D3) — see [`create_resumable`].
pub struct ResumeCtx<'a> {
    pub task: &'a str,
    /// `"plan"` or `"build"` (never `"plan_then_build"`).
    pub run_kind: &'a str,
    /// The originating `DepthMode` label.
    pub depth: &'a str,
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
    create_resumable(db, None, session_id, work_item_id, attempts_remaining, None).await
}

/// [`create`] plus the Unified Chat UI phase 6 (D3) resume context: an optional caller-supplied
/// `id` (the pre-generated launch `run_id`, keyed synchronously into the run-control registry
/// before this row exists — a fresh `Uuid` is minted when `None`) and an optional [`ResumeCtx`]
/// (persisted so `resume_run` can reconstruct a relaunch; `None` for a caller with no resume
/// context, e.g. the eval harness or a test fixture).
///
/// REVIEW FIX (CRITICAL, phase-06 review): idempotent when `id` already names an active row.
/// `resume_run` UPDATEs a paused/interrupted row in place via [`resume_reset`] (same id,
/// `deleted_at` still NULL) and THEN relaunches through the exact same code path a fresh launch
/// uses — which calls this function with that SAME `id`. A plain INSERT would collide on the
/// primary key (`UNIQUE constraint failed: pipeline_runs.id`), silently aborting the relaunch
/// before a single stage runs. Since a fresh launch's pre-generated `id` (a random `Uuid::new_v4`)
/// essentially never collides with an existing row, an existing-row hit here IS the resume case
/// by construction — return that row as-is (already reset by `resume_reset`) rather than
/// attempting a duplicate insert; `work_item_id`/`attempts_remaining`/`resume_ctx` are ignored in
/// that branch (the existing row already carries the correct post-reset values).
///
/// # Errors
/// Returns an error if `session_id`/`work_item_id` do not reference valid rows or the insert
/// fails (for a genuinely new `id`).
pub async fn create_resumable(
    db: &DbHandle,
    id: Option<&str>,
    session_id: &str,
    work_item_id: Option<&str>,
    attempts_remaining: i64,
    resume_ctx: Option<ResumeCtx<'_>>,
) -> Result<PipelineRun> {
    let owned_id;
    let id: &str = match id {
        Some(v) => v,
        None => {
            owned_id = Uuid::new_v4().to_string();
            &owned_id
        }
    };
    if let Some(existing) = get(db, id).await? {
        return Ok(existing);
    }
    let now = chrono::Utc::now().to_rfc3339();
    let (task, run_kind, depth) = match resume_ctx {
        Some(c) => (Some(c.task), Some(c.run_kind), Some(c.depth)),
        None => (None, None, None),
    };
    Ok(sqlx::query_as::<_, PipelineRun>(
        "INSERT INTO pipeline_runs
             (id, work_item_id, session_id, stage_index, status, attempt,
              attempts_remaining, task, run_kind, depth, created_at, updated_at)
         VALUES (?, ?, ?, 0, 'queued', 0, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(id)
    .bind(work_item_id)
    .bind(session_id)
    .bind(attempts_remaining)
    .bind(task)
    .bind(run_kind)
    .bind(depth)
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
             pause_reason_class = ?, updated_at = ?
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
    .bind(t.pause_reason_class)
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

/// Atomically flip a resumable row back to `running` at a FRESH per-stage attempt count (0) and
/// pipeline-global attempts budget, clearing any pause class (Unified Chat UI phase 6, D3) — the
/// one write `resume_run` performs before relaunching. The `WHERE` guard re-verifies eligibility
/// in the SAME statement that mutates the row, closing the TOCTOU window between an earlier
/// read-only guard check and this write (a concurrent reaper reap or a second `resume_run` call
/// cannot both "win"). Eligible iff `status = 'interrupted'`, or `status = 'paused'` with a
/// `pause_reason_class` of `retries_exhausted`/`explicit_stop` — never `awaiting_approval`/
/// `other`, and never a terminal/live status.
///
/// REVIEW FIX (MED, phase-06 review): `stage_index` is DELIBERATELY NOT reset here — it is
/// preserved at whatever value the row carried when it paused/was interrupted, so
/// `PipelineRunner::run`'s stage loop re-enters at the SAME stage rather than replaying every
/// earlier stage of the SAME `Pipeline` (the locked D3 semantic: "re-enter the CURRENT stage with
/// a fresh attempt budget", not "replay the whole pipeline"). This is correct for the row that is
/// actually registered/resumable (the FIRST `runner.run()` call of a launch, which
/// `create_resumable`'s idempotent-return path hands the SAME `Pipeline` shape back to on
/// relaunch — see that function's doc); a paused row from a LATER internal call within a
/// multi-run wrapper (`run_build`'s review/fix-round/ship, `run_plan`'s revise pass) is not
/// registered for a live resume at all today (a wrapper-level limitation predating this fix, not
/// introduced by it — see the phase's Deviation Log).
///
/// Returns the reset row, or `None` if `id` does not reference an active, eligible row — the
/// caller (`resume_run`) treats that as "not resumable", never an error.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn resume_reset(
    db: &DbHandle,
    id: &str,
    attempts_remaining: i64,
) -> Result<Option<PipelineRun>> {
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, PipelineRun>(
        "UPDATE pipeline_runs
         SET status = 'running', attempt = 0, attempts_remaining = ?,
             pause_reason_class = NULL, updated_at = ?
         WHERE id = ? AND deleted_at IS NULL
           AND (status = 'interrupted'
                OR (status = 'paused'
                    AND pause_reason_class IN ('retries_exhausted', 'explicit_stop')))
         RETURNING *",
    )
    .bind(attempts_remaining)
    .bind(&now)
    .bind(id)
    .fetch_optional(db.pool())
    .await?)
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

/// The most recent non-terminal (queued/running/paused/interrupted) run for `session_id`, if
/// any (Unified Chat UI phase 10). A workspace's `coding_workspaces.run_id` is only stamped
/// AFTER its driving run reaches a terminal/paused state (see `coding_workspaces::set_run_id`'s
/// doc) — so a workspace whose launch is still genuinely in flight has no `run_id` to look up
/// yet, and the Workspaces screen must fall back to this session-keyed lookup to show it as
/// "running" at all. A launch opens at most one live run per session, so the most-recent match
/// is unambiguous in practice.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn find_active_by_session(
    db: &DbHandle,
    session_id: &str,
) -> Result<Option<PipelineRun>> {
    Ok(sqlx::query_as::<_, PipelineRun>(
        "SELECT * FROM pipeline_runs
         WHERE session_id = ? AND deleted_at IS NULL
           AND status IN ('queued', 'running', 'paused', 'interrupted')
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(db.pool())
    .await?)
}

/// List runs for the Runs screen (Unified Chat UI phase 7, D6): every active row
/// (queued/running/paused/interrupted, uncapped — there are never many concurrent runs) PLUS a
/// bounded window of the most recent `terminal_limit` terminal (done/failed) rows, so history
/// growth never makes this query unbounded (Risk Assessment: "load more" pagination deferred,
/// YAGNI). Ordered active-first, newest-`updated_at`-first within each group, so a caller can
/// render the result as-is with no client-side re-sort.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_runs(db: &DbHandle, terminal_limit: i64) -> Result<Vec<PipelineRun>> {
    Ok(sqlx::query_as::<_, PipelineRun>(
        "SELECT * FROM pipeline_runs
         WHERE deleted_at IS NULL
           AND (status IN ('queued', 'running', 'paused', 'interrupted')
                OR id IN (
                  SELECT id FROM pipeline_runs
                  WHERE deleted_at IS NULL AND status IN ('done', 'failed')
                  ORDER BY updated_at DESC
                  LIMIT ?
                ))
         ORDER BY (status IN ('queued', 'running', 'paused', 'interrupted')) DESC, updated_at DESC",
    )
    .bind(terminal_limit)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::sessions;

    async fn db() -> (tempfile::TempDir, DbHandle) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        (dir, db)
    }

    async fn new_session(db: &DbHandle) -> String {
        let id = Uuid::new_v4().to_string();
        sessions::create_session(db, &id, "coding", None)
            .await
            .unwrap();
        id
    }

    /// REVIEW FIX (CRITICAL, phase-06 review): a relaunch's `create_resumable(Some(run_id))`
    /// against an id `resume_reset` already reset to `running` in place must return that SAME
    /// row rather than attempting a duplicate INSERT (which would collide on the primary key).
    #[tokio::test]
    async fn create_resumable_with_an_existing_active_id_returns_the_existing_row_not_a_collision()
    {
        let (_dir, db) = db().await;
        let session = new_session(&db).await;
        let created = create(&db, &session, None, 5).await.unwrap();

        let reset = resume_reset(&db, &created.id, 8).await;
        // Not eligible from `queued` — force it into a resumable state directly to isolate the
        // collision-repro path (this test's subject is `create_resumable`, not `resume_reset`'s
        // own eligibility guard, covered separately below).
        assert!(
            reset.unwrap().is_none(),
            "a queued row is not resume-eligible"
        );

        transition(
            &db,
            &created.id,
            RunTransition {
                stage_index: 1,
                status: "interrupted",
                attempt: 0,
                attempts_remaining: 5,
                tier_used: None,
                backend_used: None,
                egress: None,
                gate_output_digest: None,
                pause_reason_class: None,
            },
        )
        .await
        .unwrap();

        let reset = resume_reset(&db, &created.id, 8).await.unwrap().unwrap();
        assert_eq!(reset.status, "running");
        assert_eq!(
            reset.stage_index, 1,
            "resume_reset must NOT reset stage_index to 0"
        );

        // This is the exact call `PipelineRunner::run` makes on relaunch — same id, resume_ctx
        // Some (the runner always seeds one). Must succeed, not `UNIQUE constraint failed`.
        let relaunched = create_resumable(
            &db,
            Some(&created.id),
            &session,
            None,
            8,
            Some(ResumeCtx {
                task: "add a feature",
                run_kind: "build",
                depth: "normal",
            }),
        )
        .await
        .expect("must not collide on the primary key");

        assert_eq!(relaunched.id, created.id);
        assert_eq!(
            relaunched.status, "running",
            "must return the ALREADY-RESET row, not a fresh queued one"
        );
        assert_eq!(
            relaunched.stage_index, 1,
            "must preserve the resumed stage_index"
        );
    }

    /// `create_resumable` with a genuinely new (non-colliding) id still inserts normally —
    /// the existing-row short-circuit must not swallow the ordinary create path.
    #[tokio::test]
    async fn create_resumable_with_a_fresh_id_inserts_normally() {
        let (_dir, db) = db().await;
        let session = new_session(&db).await;
        let fresh_id = Uuid::new_v4().to_string();

        let created = create_resumable(&db, Some(&fresh_id), &session, None, 5, None)
            .await
            .unwrap();

        assert_eq!(created.id, fresh_id);
        assert_eq!(created.status, "queued");
        assert_eq!(created.stage_index, 0);
    }

    /// `list_runs` returns every active row plus only the bounded window of most-recent
    /// terminal rows — an OLDER terminal row past the limit must not appear.
    #[tokio::test]
    async fn list_runs_returns_all_active_plus_bounded_recent_terminal() {
        let (_dir, db) = db().await;
        let session = new_session(&db).await;

        let active = create(&db, &session, None, 5).await.unwrap();

        let mut terminal_ids = Vec::new();
        for _ in 0..3 {
            let row = create(&db, &session, None, 5).await.unwrap();
            transition(
                &db,
                &row.id,
                RunTransition {
                    stage_index: 0,
                    status: "done",
                    attempt: 1,
                    attempts_remaining: 4,
                    tier_used: None,
                    backend_used: None,
                    egress: None,
                    gate_output_digest: None,
                    pause_reason_class: None,
                },
            )
            .await
            .unwrap();
            terminal_ids.push(row.id);
            // Ensure a distinct `updated_at` ordering between iterations.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let runs = list_runs(&db, 2).await.unwrap();
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();

        assert!(
            ids.contains(&active.id.as_str()),
            "the active (queued) row must always be included"
        );
        assert!(
            !ids.contains(&terminal_ids[0].as_str()),
            "the oldest terminal row must be excluded once past the bounded window"
        );
        assert!(ids.contains(&terminal_ids[1].as_str()));
        assert!(ids.contains(&terminal_ids[2].as_str()));
        assert_eq!(runs.len(), 3, "one active + two most-recent terminal rows");
    }

    /// A soft-deleted (killed) run must never appear in `list_runs`, active or terminal.
    #[tokio::test]
    async fn list_runs_excludes_soft_deleted_rows() {
        let (_dir, db) = db().await;
        let session = new_session(&db).await;
        let row = create(&db, &session, None, 5).await.unwrap();
        soft_delete(&db, &row.id).await.unwrap();

        let runs = list_runs(&db, 50).await.unwrap();
        assert!(runs.iter().all(|r| r.id != row.id));
    }

    /// `resume_reset` preserves `stage_index` (D3: "re-enter the CURRENT stage", not the whole
    /// pipeline) while still resetting `attempt`/`attempts_remaining`/`pause_reason_class`.
    #[tokio::test]
    async fn resume_reset_preserves_stage_index_but_resets_attempt_state() {
        let (_dir, db) = db().await;
        let session = new_session(&db).await;
        let created = create(&db, &session, None, 5).await.unwrap();
        transition(
            &db,
            &created.id,
            RunTransition {
                stage_index: 2,
                status: "paused",
                attempt: 3,
                attempts_remaining: 1,
                tier_used: None,
                backend_used: None,
                egress: None,
                gate_output_digest: None,
                pause_reason_class: Some("retries_exhausted"),
            },
        )
        .await
        .unwrap();

        let reset = resume_reset(&db, &created.id, 8).await.unwrap().unwrap();
        assert_eq!(
            reset.stage_index, 2,
            "stage_index must be preserved, not reset to 0"
        );
        assert_eq!(reset.attempt, 0, "the per-stage attempt count resets");
        assert_eq!(reset.attempts_remaining, 8, "the global budget refreshes");
        assert_eq!(reset.status, "running");
        assert!(reset.pause_reason_class.is_none());
    }
}
