use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub pattern: String,
    pub steps: String,
    pub confidence: f64,
    pub use_count: i64,
    pub last_used_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
    pub archived_at: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct TaskTrace {
    pub id: String,
    pub session_id: String,
    pub task_description: String,
    pub tool_calls: String,
    pub outcome: String,
    pub duration_ms: Option<i64>,
    pub created_at: String,
    // --- Harness Completion phase 5: per-turn telemetry (migration 0017) ---
    // All nullable/additive — a row from before this migration, or a turn where the
    // value genuinely could not be computed, reads back as `None`, never a fabricated
    // default (see the `estimate_tokens`-vs-`None` contract in `haily-core::agent`).
    pub model_tier: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub tool_call_count: Option<i64>,
    pub approval_requested: Option<bool>,
    pub approval_denied: Option<bool>,
    pub undo_within_5min: Option<bool>,
    /// `None` means "no label signal fired this turn" — the anti-reinforcement
    /// safety invariant: `label_source IS NULL` MUST NOT drive `update_skill_confidence`
    /// (see `haily_kms::skills::derive_label`).
    pub label_source: Option<String>,
    pub label_confidence: Option<f64>,
    pub delegate_overhead_ms: Option<i64>,
    /// Auto Model Routing R1 join key (migration 0031) — correlates this trace with its
    /// `routing_decisions` row(s). `None` for any trace inserted before this migration, or
    /// any caller that has not yet been wired to pass the turn's id (see `TraceMetrics::turn_id`).
    pub turn_id: Option<String>,
}

/// Per-turn telemetry to persist alongside a trace's outcome (Harness Completion
/// phase 5). Grouped so `insert_trace` stays within a sane arity — mirrors the
/// `NewAction`/`SubTurnRequest` convention already used elsewhere in this codebase
/// for the same reason.
///
/// Every field is optional at the call site: `haily-core::agent` passes `None` for
/// any metric it could not compute this turn (e.g. no real token-usage API surfaced
/// by the current LLM backend) rather than fabricating a value — see CLAUDE.md's
/// "real code only" rule.
#[derive(Debug, Clone, Default)]
pub struct TraceMetrics<'a> {
    pub model_tier: Option<&'a str>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub tool_call_count: Option<i64>,
    pub approval_requested: Option<bool>,
    pub approval_denied: Option<bool>,
    pub undo_within_5min: Option<bool>,
    pub label_source: Option<&'a str>,
    pub label_confidence: Option<f64>,
    pub delegate_overhead_ms: Option<i64>,
    /// Auto Model Routing R1 join key (migration 0031) — the routing decision log's
    /// `turn_id`. Every existing caller passes `None` via `..Default::default()`/`Default`
    /// until the routing decision emitter (Phase 4) wires the real value in.
    pub turn_id: Option<&'a str>,
}

pub async fn insert_trace(
    db: &DbHandle,
    session_id: &str,
    task_description: &str,
    tool_calls_json: &str,
    outcome: &str,
    duration_ms: Option<i64>,
    metrics: TraceMetrics<'_>,
) -> Result<TaskTrace> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, TaskTrace>(
        "INSERT INTO kms_task_traces
             (id, session_id, task_description, tool_calls, outcome, duration_ms, created_at,
              model_tier, prompt_tokens, completion_tokens, tool_call_count,
              approval_requested, approval_denied, undo_within_5min,
              label_source, label_confidence, delegate_overhead_ms, turn_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(session_id)
    .bind(task_description)
    .bind(tool_calls_json)
    .bind(outcome)
    .bind(duration_ms)
    .bind(&now)
    .bind(metrics.model_tier)
    .bind(metrics.prompt_tokens)
    .bind(metrics.completion_tokens)
    .bind(metrics.tool_call_count)
    .bind(metrics.approval_requested)
    .bind(metrics.approval_denied)
    .bind(metrics.undo_within_5min)
    .bind(metrics.label_source)
    .bind(metrics.label_confidence)
    .bind(metrics.delegate_overhead_ms)
    .bind(metrics.turn_id)
    .fetch_one(db.pool())
    .await?)
}

pub async fn recent_traces(db: &DbHandle, limit: i64) -> Result<Vec<TaskTrace>> {
    Ok(sqlx::query_as::<_, TaskTrace>(
        "SELECT * FROM kms_task_traces ORDER BY created_at DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db.pool())
    .await?)
}

/// Insert a skill, or un-archive a same-name archived skill (fresh synthesis is
/// evidence the pattern is alive again). Skipped silently if an active (non-archived)
/// row with `name` already exists (unique index guard on `name`).
///
/// # Errors
/// Returns an error if the DB insert/update/fetch fails.
pub async fn insert_skill(
    db: &DbHandle,
    name: &str,
    description: &str,
    pattern: &str,
    steps_json: &str,
) -> Result<Skill> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT OR IGNORE INTO kms_skills
             (id, name, description, pattern, steps, confidence, use_count, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 1.0, 0, ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(description)
    .bind(pattern)
    .bind(steps_json)
    .bind(&now)
    .bind(&now)
    .execute(db.pool())
    .await?;

    // The UNIQUE index on `name` means INSERT OR IGNORE leaves no fresh row when a
    // same-name row (active or archived) already exists — fetch_one against
    // `archived_at IS NULL` would then error RowNotFound for the archived case.
    // fetch_optional + explicit branch turns that into un-archival instead.
    match sqlx::query_as::<_, Skill>(
        "SELECT * FROM kms_skills WHERE name = ? AND deleted_at IS NULL AND archived_at IS NULL",
    )
    .bind(name)
    .fetch_optional(db.pool())
    .await?
    {
        Some(active) => Ok(active),
        None => {
            // Either the row is archived (resurrect it) or somehow missing —
            // in the missing case this UPDATE affects 0 rows and the final
            // fetch_one below surfaces a clear error instead of silent resurrection.
            sqlx::query(
                "UPDATE kms_skills
                 SET archived_at = NULL, confidence = 1.0, updated_at = ?
                 WHERE name = ? AND deleted_at IS NULL",
            )
            .bind(&now)
            .bind(name)
            .execute(db.pool())
            .await?;

            Ok(sqlx::query_as::<_, Skill>(
                "SELECT * FROM kms_skills WHERE name = ? AND deleted_at IS NULL",
            )
            .bind(name)
            .fetch_one(db.pool())
            .await?)
        }
    }
}

pub async fn increment_use_count(db: &DbHandle, id: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE kms_skills
         SET use_count = use_count + 1, last_used_at = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Traces created at or after `since` (RFC3339). Capped at 500 rows (H2 guard).
pub async fn traces_since(db: &DbHandle, since: &str) -> Result<Vec<TaskTrace>> {
    Ok(sqlx::query_as::<_, TaskTrace>(
        "SELECT * FROM kms_task_traces WHERE created_at >= ? ORDER BY created_at DESC LIMIT 500",
    )
    .bind(since)
    .fetch_all(db.pool())
    .await?)
}

/// Fetch a single active skill by ID — used for targeted EMA updates.
pub async fn get_skill(db: &DbHandle, id: &str) -> Result<Option<Skill>> {
    Ok(sqlx::query_as::<_, Skill>(
        "SELECT * FROM kms_skills WHERE id = ? AND deleted_at IS NULL AND archived_at IS NULL",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await?)
}

/// Fetch a skill by ID regardless of archived/deleted state — unlike `get_skill`
/// (which exists for "targeted EMA updates" on ACTIVE skills only), this is for
/// callers that need to observe the archival outcome itself (e.g. a corroboration-
/// floor test asserting a skill was or was not archived by `apply_skill_decay`).
pub async fn get_skill_any_state(db: &DbHandle, id: &str) -> Result<Option<Skill>> {
    Ok(sqlx::query_as::<_, Skill>("SELECT * FROM kms_skills WHERE id = ?")
        .bind(id)
        .fetch_optional(db.pool())
        .await?)
}

/// Top-N active skills by confidence — used to inject skills into the system prompt.
pub async fn active_skills_top(db: &DbHandle, limit: i64) -> Result<Vec<Skill>> {
    Ok(sqlx::query_as::<_, Skill>(
        "SELECT * FROM kms_skills
         WHERE deleted_at IS NULL AND archived_at IS NULL
         ORDER BY confidence DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db.pool())
    .await?)
}

/// Atomic EMA confidence update: `confidence = alpha*reward + (1-alpha)*confidence`.
///
/// The whole formula runs as one UPDATE so concurrent calls for the same skill each
/// read-modify-write against the DB's current row version instead of a value snapshotted
/// in Rust — two concurrent calls both land, rather than one clobbering the other.
pub async fn update_skill_confidence(
    db: &DbHandle,
    id: &str,
    reward: f64,
    alpha: f64,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE kms_skills
         SET confidence = MIN(1.0, MAX(0.0, ? * ? + (1.0 - ?) * confidence)),
             updated_at = ?
         WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(alpha)
    .bind(reward)
    .bind(alpha)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Active (non-deleted, non-archived) skills.
pub async fn active_skills(db: &DbHandle) -> Result<Vec<Skill>> {
    Ok(sqlx::query_as::<_, Skill>(
        "SELECT * FROM kms_skills
         WHERE deleted_at IS NULL AND archived_at IS NULL
         ORDER BY confidence DESC",
    )
    .fetch_all(db.pool())
    .await?)
}

/// Preference key (via `queries::meta`) recording the RFC3339 timestamp of the last
/// successful decay run — guards `apply_exponential_decay` against being fired twice
/// within `MIN_DECAY_INTERVAL_HOURS` (e.g. an overlapping worker restart).
const LAST_DECAY_RUN_KEY: &str = "kms.skills.last_decay_run";
const MIN_DECAY_INTERVAL_HOURS: i64 = 20;

/// Apply exponential decay to the confidence of every active skill. Does NOT archive
/// anything by itself (Harness Completion phase 5, M1 review fix) — decaying
/// confidence and deciding to archive are now separate steps so the caller
/// (`haily_kms::skills::apply_skill_decay`) can interpose a corroboration check
/// between them (see that function's doc comment for why: archival must not be
/// driven by the SAME weak signal that produced the low confidence in the first
/// place — the same-signal-produces-and-validates loop researcher-03 §2 warns
/// against applies to archival exactly as it does to the EMA).
/// `lambda` ≈ 0.693/24 gives half-life of 24 h when called hourly.
///
/// No-op (returns `Ok(0)`) if less than `MIN_DECAY_INTERVAL_HOURS` have elapsed since
/// the last successful run, making the operation idempotent under duplicate/overlapping
/// scheduler ticks.
pub async fn apply_exponential_decay(db: &DbHandle, lambda: f64) -> Result<usize> {
    // Atomically CLAIM the decay slot before doing any work. A conditional upsert of the
    // run-timestamp — insert on first run, else update only when the stored run is older
    // than the guard window — collapses the previous read-then-act guard into one statement,
    // so two overlapping workers cannot both pass the check (the TOCTOU the guard exists to
    // close). `rows_affected == 0` means another run already claimed within the window.
    // RFC3339 UTC strings from `to_rfc3339()` share a fixed format, so lexical `<` orders them
    // by time. The claim is written BEFORE decay: a failed decay simply skips this cycle
    // rather than allowing a duplicate, which is the safer trade-off for a periodic worker.
    let now = chrono::Utc::now().to_rfc3339();
    let threshold =
        (chrono::Utc::now() - chrono::Duration::hours(MIN_DECAY_INTERVAL_HOURS)).to_rfc3339();
    let claim_id = uuid::Uuid::new_v4().to_string();
    let claimed = sqlx::query(
        "INSERT INTO kms_preferences (id, key, value, confidence, source, created_at, updated_at)
         VALUES (?, ?, ?, 1.0, 'system', ?, ?)
         ON CONFLICT(key) DO UPDATE SET
             value      = excluded.value,
             updated_at = excluded.updated_at
         WHERE kms_preferences.value < ?",
    )
    .bind(&claim_id)
    .bind(LAST_DECAY_RUN_KEY)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&threshold)
    .execute(db.pool())
    .await?
    .rows_affected();

    if claimed == 0 {
        return Ok(0);
    }

    let factor = (-lambda).exp();
    let rows = sqlx::query(
        "UPDATE kms_skills
         SET confidence = ROUND(confidence * ?, 4),
             updated_at = ?
         WHERE deleted_at IS NULL AND archived_at IS NULL",
    )
    .bind(factor)
    .bind(&now)
    .execute(db.pool())
    .await?
    .rows_affected();

    Ok(rows as usize)
}

/// Active, non-archived skills whose confidence is strictly below `archive_below` —
/// the candidate pool `apply_skill_decay` checks for archival corroboration (M1)
/// AFTER `apply_exponential_decay` has already applied this cycle's decay. Kept as a
/// separate read (not folded into the decay UPDATE) so the corroboration check can
/// run in Rust (Jaccard matching has no SQL equivalent) between "decay applied" and
/// "archive decided."
pub async fn skills_below_confidence(db: &DbHandle, archive_below: f64) -> Result<Vec<Skill>> {
    Ok(sqlx::query_as::<_, Skill>(
        "SELECT * FROM kms_skills
         WHERE deleted_at IS NULL AND archived_at IS NULL AND confidence < ?",
    )
    .bind(archive_below)
    .fetch_all(db.pool())
    .await?)
}

/// Archive one skill by id. A skill already archived/deleted is a silent no-op (the
/// `WHERE` clause matches nothing) — safe to call from `apply_skill_decay`'s
/// corroboration loop even under a benign race with another decay cycle.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn archive_skill(db: &DbHandle, id: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE kms_skills SET archived_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL AND archived_at IS NULL",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Negative-labeled traces (`outcome IN ('failure','partial')` — the same two
/// `TaskOutcome` values `derive_label`'s `ToolErrorRatio`/`UndoWithinN` branches can
/// produce) recorded since `since`, across ALL sessions. This is the candidate pool
/// `apply_skill_decay`'s corroboration check matches against each low-confidence
/// skill's `description` (Jaccard, same bar as `find_matching_skill`) — capped at 500
/// rows for the same reason `traces_since` is (H2 guard: a periodic worker read must
/// stay cheap regardless of how much history has accumulated).
pub async fn negative_traces_since(db: &DbHandle, since: &str) -> Result<Vec<TaskTrace>> {
    Ok(sqlx::query_as::<_, TaskTrace>(
        "SELECT * FROM kms_task_traces
         WHERE created_at >= ? AND outcome IN ('failure', 'partial')
         ORDER BY created_at DESC LIMIT 500",
    )
    .bind(since)
    .fetch_all(db.pool())
    .await?)
}

// ---------------------------------------------------------------------------
// Harness Completion phase 5: feedback-to-trace join (m2), undo-within-N label
// (m4), and daily rollup/retention.
// ---------------------------------------------------------------------------

/// Most recent trace for `session_id` — the join target for a `Negative`/`Correction`
/// feedback signal firing on the NEXT user message (m2: caller is responsible for
/// verifying the signal itself came from a genuine user message before calling
/// `downgrade_trace`; this query has no attribution logic of its own).
pub async fn most_recent_trace(db: &DbHandle, session_id: &str) -> Result<Option<TaskTrace>> {
    Ok(sqlx::query_as::<_, TaskTrace>(
        "SELECT * FROM kms_task_traces WHERE session_id = ? ORDER BY created_at DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(db.pool())
    .await?)
}

/// Overwrite a trace's outcome + label after the fact — used when a genuine-user
/// `Negative`/`Correction` signal on turn N+1 downgrades turn N's already-inserted
/// trace (m2). `label_source`/`label_confidence` are set here (not left as whatever
/// `insert_trace` originally recorded), since the feedback signal is now the
/// authoritative label for this trace going forward.
///
/// # Errors
/// Returns an error if the update fails. Silently succeeds (0 rows) if `id` does
/// not exist.
pub async fn downgrade_trace(
    db: &DbHandle,
    id: &str,
    outcome: &str,
    label_source: &str,
    label_confidence: f64,
) -> Result<()> {
    sqlx::query(
        "UPDATE kms_task_traces
         SET outcome = ?, label_source = ?, label_confidence = ?
         WHERE id = ?",
    )
    .bind(outcome)
    .bind(label_source)
    .bind(label_confidence)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Apply a deterministic GateResult label (Sub-Agent + Skill Architecture phase 8) to a task
/// trace: set its `outcome` to the gate verdict AND stamp `label_source='gate_result'`,
/// `label_confidence=0.9`. The WHERE guard `(label_source IS NULL OR label_source !=
/// 'explicit_feedback')` is the anti-reinforcement precedence invariant (LOCKED decision #4):
/// a gate result NEVER overwrites an explicit human-feedback label already on the trace, but
/// freely labels an unlabeled trace or supersedes a weaker heuristic label. Mirrors the
/// `gate_label_supersedes` predicate in `haily_kms::skills` (kept in lockstep with it).
///
/// `outcome` is the caller-mapped `TaskOutcome::as_str()` value (`success`/`failure`) matching
/// the gate pass/fail. Returns `true` iff a row was actually re-labeled (`false` = row absent
/// OR an explicit-feedback label protected it).
///
/// # Errors
/// Returns an error if the update fails.
pub async fn apply_gate_result_label(db: &DbHandle, trace_id: &str, outcome: &str) -> Result<bool> {
    let rows = sqlx::query(
        "UPDATE kms_task_traces
         SET outcome = ?, label_source = 'gate_result', label_confidence = 0.9
         WHERE id = ? AND (label_source IS NULL OR label_source != 'explicit_feedback')",
    )
    .bind(outcome)
    .bind(trace_id)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// m4 exact undo predicate: EXISTS a same-`session_id` `action_journal` row with
/// `undo_status='undone'` AND `undone_at` within `window_minutes` of `created_at`
/// (the action being recorded — NOT the undo row, since undo mutates the ORIGINAL
/// row in place; there is no distinct "undo row" to join against). RFC3339 UTC
/// timestamps compare correctly as `julianday()` inputs regardless of the `Z`
/// suffix SQLite's datetime functions expect, since `to_rfc3339()` always emits an
/// explicit UTC offset.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn undo_within_n_min(
    db: &DbHandle,
    session_id: &str,
    created_at: &str,
    window_minutes: i64,
) -> Result<bool> {
    let row = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM action_journal
         WHERE session_id = ?
           AND undo_status = 'undone'
           AND undone_at IS NOT NULL
           AND ABS(julianday(undone_at) - julianday(?)) <= (? / 1440.0)",
    )
    .bind(session_id)
    .bind(created_at)
    .bind(window_minutes)
    .fetch_one(db.pool())
    .await?;
    Ok(row.0 > 0)
}

/// One row of the daily rollup aggregation (Harness Completion phase 5, researcher-03
/// §3). `model_tier` is normalized to `''` for "no tier recorded" BEFORE calling
/// this (see `compute_daily_rollup`) so the `UNIQUE(date, model_tier)` upsert target
/// is well-defined — SQLite's UNIQUE index does not dedupe NULLs.
#[derive(Debug, Clone, FromRow)]
pub struct DailyRollup {
    pub id: String,
    pub date: String,
    pub model_tier: String,
    pub count: i64,
    pub success_count: i64,
    pub partial_count: i64,
    pub failure_count: i64,
    pub unknown_count: i64,
    pub avg_duration_ms: Option<f64>,
    pub avg_prompt_tokens: Option<f64>,
    pub avg_completion_tokens: Option<f64>,
    pub undo_count: i64,
    // --- Phase 8 (Activate & Measure), migration 0020 ---
    // SUMs of the already-recorded per-turn `kms_task_traces.approval_requested`/
    // `approval_denied` booleans (migration 0017) — see that migration's doc comment
    // for why `0` (not `NULL`) is the honest default here.
    pub approval_requested_count: i64,
    pub approval_denied_count: i64,
    pub created_at: String,
}

/// One `GROUP BY tier` bucket of the daily aggregation query — a dedicated
/// `FromRow` struct instead of a bare tuple so adding a column (e.g. migration
/// 0020's approval counts) is a one-line addition here rather than growing an
/// already-long tuple type past comfortable readability.
#[derive(Debug, FromRow)]
struct RollupAggRow {
    tier: String,
    cnt: i64,
    success_count: i64,
    partial_count: i64,
    failure_count: i64,
    unknown_count: i64,
    avg_duration_ms: Option<f64>,
    avg_prompt_tokens: Option<f64>,
    avg_completion_tokens: Option<f64>,
    undo_count: i64,
    approval_requested_count: i64,
    approval_denied_count: i64,
}

/// Aggregate every `kms_task_traces` row whose `created_at` date (UTC, `YYYY-MM-DD`)
/// equals `date`, grouped by `model_tier` (NULL coalesced to `''`), and upsert the
/// result into `kms_daily_rollup`. Idempotent: rerunning for the same `date` replaces
/// (not adds to) that date's rows, so a worker restart mid-cycle cannot double-count.
///
/// `outcome` is matched case-sensitively against `TaskOutcome::as_str()`'s exact
/// values (`success`/`partial`/`failure`); anything else (including a legacy/unknown
/// string) falls into `unknown_count` rather than being silently dropped from `count`.
///
/// # Errors
/// Returns an error if the aggregation query or upsert fails.
pub async fn compute_daily_rollup(db: &DbHandle, date: &str) -> Result<usize> {
    let day_start = format!("{date}T00:00:00");
    let day_end = format!("{date}T23:59:59.999999");

    let rows = sqlx::query_as::<_, RollupAggRow>(
        "SELECT
             COALESCE(model_tier, '')                                        AS tier,
             COUNT(*)                                                        AS cnt,
             SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END)            AS success_count,
             SUM(CASE WHEN outcome = 'partial' THEN 1 ELSE 0 END)            AS partial_count,
             SUM(CASE WHEN outcome = 'failure' THEN 1 ELSE 0 END)            AS failure_count,
             SUM(CASE WHEN outcome NOT IN ('success','partial','failure') THEN 1 ELSE 0 END) AS unknown_count,
             AVG(duration_ms)                                                AS avg_duration_ms,
             AVG(prompt_tokens)                                              AS avg_prompt_tokens,
             AVG(completion_tokens)                                          AS avg_completion_tokens,
             SUM(CASE WHEN undo_within_5min = 1 THEN 1 ELSE 0 END)           AS undo_count,
             SUM(CASE WHEN approval_requested = 1 THEN 1 ELSE 0 END)         AS approval_requested_count,
             SUM(CASE WHEN approval_denied = 1 THEN 1 ELSE 0 END)            AS approval_denied_count
         FROM kms_task_traces
         WHERE created_at >= ? AND created_at <= ?
         GROUP BY tier",
    )
    .bind(&day_start)
    .bind(&day_end)
    .fetch_all(db.pool())
    .await?;

    let now = chrono::Utc::now().to_rfc3339();
    let mut upserted = 0usize;
    for row in rows {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO kms_daily_rollup
                 (id, date, model_tier, count, success_count, partial_count, failure_count,
                  unknown_count, avg_duration_ms, avg_prompt_tokens, avg_completion_tokens,
                  undo_count, approval_requested_count, approval_denied_count, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(date, model_tier) DO UPDATE SET
                 count                     = excluded.count,
                 success_count             = excluded.success_count,
                 partial_count             = excluded.partial_count,
                 failure_count             = excluded.failure_count,
                 unknown_count             = excluded.unknown_count,
                 avg_duration_ms           = excluded.avg_duration_ms,
                 avg_prompt_tokens         = excluded.avg_prompt_tokens,
                 avg_completion_tokens     = excluded.avg_completion_tokens,
                 undo_count                = excluded.undo_count,
                 approval_requested_count  = excluded.approval_requested_count,
                 approval_denied_count     = excluded.approval_denied_count",
        )
        .bind(&id)
        .bind(date)
        .bind(&row.tier)
        .bind(row.cnt)
        .bind(row.success_count)
        .bind(row.partial_count)
        .bind(row.failure_count)
        .bind(row.unknown_count)
        .bind(row.avg_duration_ms)
        .bind(row.avg_prompt_tokens)
        .bind(row.avg_completion_tokens)
        .bind(row.undo_count)
        .bind(row.approval_requested_count)
        .bind(row.approval_denied_count)
        .bind(&now)
        .execute(db.pool())
        .await?;
        upserted += 1;
    }
    Ok(upserted)
}

/// Most recent date present in `kms_daily_rollup` (`YYYY-MM-DD`, lexicographically
/// sortable so `MAX` is also date-max), or `None` on a fresh install with no rollup
/// rows yet. Drives the M7b backfill loop (`haily_proactive::daily_rollup`): resuming
/// from the day AFTER this one, rather than always re-targeting a fixed
/// `now - 1 day`, is what lets a slept-through gap day get rolled instead of skipped
/// forever.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn latest_rollup_date(db: &DbHandle) -> Result<Option<String>> {
    let row: (Option<String>,) = sqlx::query_as("SELECT MAX(date) FROM kms_daily_rollup")
        .fetch_one(db.pool())
        .await?;
    Ok(row.0)
}

/// Rollup rows for a given date, newest-tier-first is not meaningful here — returned
/// in whatever order SQLite yields them (small result set, at most one row per tier).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn rollup_for_date(db: &DbHandle, date: &str) -> Result<Vec<DailyRollup>> {
    Ok(
        sqlx::query_as::<_, DailyRollup>("SELECT * FROM kms_daily_rollup WHERE date = ?")
            .bind(date)
            .fetch_all(db.pool())
            .await?,
    )
}

/// Delete raw `kms_task_traces` rows older than `retention_days` — called AFTER
/// `compute_daily_rollup` has aggregated them, so the rollup is the durable record of
/// anything this removes. Returns the number of rows deleted.
///
/// # Errors
/// Returns an error if the delete fails.
pub async fn delete_traces_older_than(db: &DbHandle, retention_days: i64) -> Result<u64> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();
    let rows = sqlx::query("DELETE FROM kms_task_traces WHERE created_at < ?")
        .bind(&cutoff)
        .execute(db.pool())
        .await?
        .rows_affected();
    Ok(rows)
}

#[cfg(test)]
mod gate_label_tests {
    //! Phase 8: `apply_gate_result_label` precedence — a deterministic gate label freely labels
    //! an unlabeled trace but NEVER overwrites an explicit human-feedback label (LOCKED #4).
    use super::*;
    use crate::queries::sessions;

    async fn db_with_session() -> (DbHandle, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        let session_id = Uuid::new_v4().to_string();
        sessions::create_session(&db, &session_id, "test", None).await.unwrap();
        (db, session_id, dir)
    }

    fn metrics_with_label(source: Option<&'static str>) -> TraceMetrics<'static> {
        TraceMetrics {
            label_source: source,
            label_confidence: source.map(|_| 0.9),
            ..TraceMetrics::default()
        }
    }

    #[tokio::test]
    async fn gate_label_applies_to_an_unlabeled_trace() {
        let (db, sid, _dir) = db_with_session().await;
        let t = insert_trace(&db, &sid, "build stage", "[]", "success", Some(1), metrics_with_label(None))
            .await
            .unwrap();
        assert_eq!(t.label_source, None, "precondition: trace starts unlabeled");

        let changed = apply_gate_result_label(&db, &t.id, "failure").await.unwrap();
        assert!(changed, "an unlabeled trace must accept the gate label");

        let after = recent_traces(&db, 5).await.unwrap();
        let after = after.iter().find(|r| r.id == t.id).unwrap();
        assert_eq!(after.label_source.as_deref(), Some("gate_result"));
        assert_eq!(after.label_confidence, Some(0.9));
        assert_eq!(after.outcome, "failure", "outcome must reflect the gate verdict");
    }

    #[tokio::test]
    async fn gate_label_never_overwrites_explicit_feedback() {
        let (db, sid, _dir) = db_with_session().await;
        let t = insert_trace(
            &db,
            &sid,
            "build stage",
            "[]",
            "failure",
            Some(1),
            metrics_with_label(Some("explicit_feedback")),
        )
        .await
        .unwrap();

        let changed = apply_gate_result_label(&db, &t.id, "success").await.unwrap();
        assert!(!changed, "an explicit_feedback label must protect the trace from a gate relabel");

        let after = recent_traces(&db, 5).await.unwrap();
        let after = after.iter().find(|r| r.id == t.id).unwrap();
        assert_eq!(
            after.label_source.as_deref(),
            Some("explicit_feedback"),
            "explicit feedback must survive — a gate result never overwrites it"
        );
        assert_eq!(after.outcome, "failure", "outcome must NOT be rewritten when the label is protected");
    }
}
