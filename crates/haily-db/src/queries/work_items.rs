use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct WorkItem {
    pub id: String,
    pub session_id: String,
    pub title: String,
    pub status: String,
    pub phase: Option<String>,
    pub progress: i64,
    pub checkpoint: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Create a new work item in queued state.
///
/// # Errors
/// Returns an error if the session_id does not reference a valid session or the insert fails.
pub async fn create(db: &DbHandle, session_id: &str, title: &str) -> Result<WorkItem> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, WorkItem>(
        "INSERT INTO work_items
             (id, session_id, title, status, progress, created_at, updated_at)
         VALUES (?, ?, ?, 'queued', 0, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(session_id)
    .bind(title)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// Transition to running and record start time.
///
/// # Errors
/// Returns an error if the DB update fails. Silently succeeds if no row matches `id`.
pub async fn start(db: &DbHandle, id: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE work_items
         SET status = 'running', started_at = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Save checkpoint data after a tool call.
///
/// `phase` is typically the last tool name, `progress` is 0–100,
/// and `checkpoint_json` is opaque serialized state for resumption.
///
/// # Errors
/// Returns an error if the DB update fails. Silently succeeds if no row matches `id`.
pub async fn checkpoint(
    db: &DbHandle,
    id: &str,
    phase: Option<&str>,
    progress: i64,
    checkpoint_json: &str,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE work_items
         SET phase = ?, progress = ?, checkpoint = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(phase)
    .bind(progress)
    .bind(checkpoint_json)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Mark as successfully completed.
///
/// # Errors
/// Returns an error if the DB update fails. Silently succeeds if no row matches `id`.
pub async fn complete(db: &DbHandle, id: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE work_items
         SET status = 'done', completed_at = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Mark as failed with an error message.
///
/// # Errors
/// Returns an error if the DB update fails. Silently succeeds if no row matches `id`.
pub async fn fail(db: &DbHandle, id: &str, error: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE work_items
         SET status = 'failed', error = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(error)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Mark as interrupted (called on shutdown or by the stale-reset sweep).
///
/// # Errors
/// Returns an error if the DB update fails. Silently succeeds if no row matches `id`.
pub async fn mark_interrupted(db: &DbHandle, id: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE work_items
         SET status = 'interrupted', updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// List all non-terminal work items (queued, running, paused, interrupted).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_active(db: &DbHandle) -> Result<Vec<WorkItem>> {
    Ok(sqlx::query_as::<_, WorkItem>(
        "SELECT * FROM work_items
         WHERE status IN ('queued', 'running', 'paused', 'interrupted')
         ORDER BY created_at ASC",
    )
    .fetch_all(db.pool())
    .await?)
}

/// List only interrupted items (shown to user on startup).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_interrupted(db: &DbHandle) -> Result<Vec<WorkItem>> {
    Ok(sqlx::query_as::<_, WorkItem>(
        "SELECT * FROM work_items
         WHERE status = 'interrupted'
         ORDER BY created_at ASC",
    )
    .fetch_all(db.pool())
    .await?)
}

/// Get a single work item by id.
///
/// Returns `None` if no item with the given id exists.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get(db: &DbHandle, id: &str) -> Result<Option<WorkItem>> {
    Ok(sqlx::query_as::<_, WorkItem>(
        "SELECT * FROM work_items WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await?)
}

/// On startup: reset any items stuck in 'running' state to 'interrupted'.
///
/// Items remain in 'running' when the process exits without a clean shutdown.
/// Returns the number of items reset.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn reset_stale_running(db: &DbHandle) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE work_items
         SET status = 'interrupted', updated_at = ?
         WHERE status = 'running'",
    )
    .bind(&now)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows)
}
