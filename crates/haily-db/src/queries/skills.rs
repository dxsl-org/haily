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
}

pub async fn insert_trace(
    db: &DbHandle,
    session_id: &str,
    task_description: &str,
    tool_calls_json: &str,
    outcome: &str,
    duration_ms: Option<i64>,
) -> Result<TaskTrace> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, TaskTrace>(
        "INSERT INTO kms_task_traces
             (id, session_id, task_description, tool_calls, outcome, duration_ms, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(session_id)
    .bind(task_description)
    .bind(tool_calls_json)
    .bind(outcome)
    .bind(duration_ms)
    .bind(&now)
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
pub async fn update_skill_confidence(db: &DbHandle, id: &str, reward: f64, alpha: f64) -> Result<()> {
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

/// Apply exponential decay to all active skills. Archive those below `archive_below`.
/// `lambda` ≈ 0.693/24 gives half-life of 24 h when called hourly.
///
/// No-op (returns `Ok(0)`) if less than `MIN_DECAY_INTERVAL_HOURS` have elapsed since
/// the last successful run, making the operation idempotent under duplicate/overlapping
/// scheduler ticks.
pub async fn apply_exponential_decay(
    db: &DbHandle,
    lambda: f64,
    archive_below: f64,
) -> Result<usize> {
    // Atomically CLAIM the decay slot before doing any work. A conditional upsert of the
    // run-timestamp — insert on first run, else update only when the stored run is older
    // than the guard window — collapses the previous read-then-act guard into one statement,
    // so two overlapping workers cannot both pass the check (the TOCTOU the guard exists to
    // close). `rows_affected == 0` means another run already claimed within the window.
    // RFC3339 UTC strings from `to_rfc3339()` share a fixed format, so lexical `<` orders them
    // by time. The claim is written BEFORE decay: a failed decay simply skips this cycle
    // rather than allowing a duplicate, which is the safer trade-off for a periodic worker.
    let now = chrono::Utc::now().to_rfc3339();
    let threshold = (chrono::Utc::now() - chrono::Duration::hours(MIN_DECAY_INTERVAL_HOURS))
        .to_rfc3339();
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
         SET confidence  = ROUND(confidence * ?, 4),
             archived_at = CASE WHEN (confidence * ?) < ? THEN ? ELSE archived_at END,
             updated_at  = ?
         WHERE deleted_at IS NULL AND archived_at IS NULL",
    )
    .bind(factor)
    .bind(factor)
    .bind(archive_below)
    .bind(&now)
    .bind(&now)
    .execute(db.pool())
    .await?
    .rows_affected();

    Ok(rows as usize)
}
