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

pub async fn insert_skill(
    db: &DbHandle,
    name: &str,
    description: &str,
    pattern: &str,
    steps_json: &str,
) -> Result<Skill> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, Skill>(
        "INSERT INTO kms_skills
             (id, name, description, pattern, steps, confidence, use_count, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 1.0, 0, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(name)
    .bind(description)
    .bind(pattern)
    .bind(steps_json)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
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

/// Traces created at or after `since` (RFC3339).
pub async fn traces_since(db: &DbHandle, since: &str) -> Result<Vec<TaskTrace>> {
    Ok(sqlx::query_as::<_, TaskTrace>(
        "SELECT * FROM kms_task_traces WHERE created_at >= ? ORDER BY created_at DESC",
    )
    .bind(since)
    .fetch_all(db.pool())
    .await?)
}

/// EMA confidence update: new_conf is the pre-computed value (caller owns the formula).
pub async fn update_skill_confidence(db: &DbHandle, id: &str, new_conf: f64) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE kms_skills SET confidence = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(new_conf)
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

/// Apply exponential decay to all active skills. Archive those below `archive_below`.
/// `lambda` ≈ 0.693/24 gives half-life of 24 h when called hourly.
pub async fn apply_exponential_decay(
    db: &DbHandle,
    lambda: f64,
    archive_below: f64,
) -> Result<usize> {
    let now = chrono::Utc::now().to_rfc3339();
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
