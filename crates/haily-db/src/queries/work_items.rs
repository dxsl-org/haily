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
    pub deleted_at: Option<String>,
    /// Workspace-relative path to the plan artifact the Plan Pipeline rendered for this item
    /// (e.g. `.agents/<slug>/plan.md`); `None` until a plan is linked (migration 0027, P5).
    pub plan_path: Option<String>,
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
/// Returns an error if the DB update fails. Silently succeeds if no row matches `id`
/// (including one that has since been soft-deleted).
pub async fn start(db: &DbHandle, id: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE work_items
         SET status = 'running', started_at = ?, updated_at = ?
         WHERE id = ? AND deleted_at IS NULL",
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
/// Returns an error if the DB update fails. Silently succeeds if no row matches `id`
/// (including one that has since been soft-deleted).
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
         WHERE id = ? AND deleted_at IS NULL",
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

/// Link a rendered plan artifact to this work item (Plan Pipeline, P5). `plan_path` is a
/// workspace-relative path (e.g. `.agents/<slug>/plan.md`), the explicit linkage the Build
/// pipeline (P6) resolves rather than re-deriving the slug.
///
/// Returns `true` if an active row was updated, `false` if `id` did not match an active row
/// (already deleted, or never existed) — mirrors the `rows_affected()` idiom used by
/// `soft_delete` so a caller can detect a vanished item without a separate SELECT.
///
/// # Errors
/// Returns an error if the DB update fails.
pub async fn link_plan(db: &DbHandle, id: &str, plan_path: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE work_items
         SET plan_path = ?, updated_at = ?
         WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(plan_path)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// Mark as successfully completed.
///
/// # Errors
/// Returns an error if the DB update fails. Silently succeeds if no row matches `id`
/// (including one that has since been soft-deleted).
pub async fn complete(db: &DbHandle, id: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE work_items
         SET status = 'done', completed_at = ?, updated_at = ?
         WHERE id = ? AND deleted_at IS NULL",
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
/// Returns an error if the DB update fails. Silently succeeds if no row matches `id`
/// (including one that has since been soft-deleted).
pub async fn fail(db: &DbHandle, id: &str, error: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE work_items
         SET status = 'failed', error = ?, updated_at = ?
         WHERE id = ? AND deleted_at IS NULL",
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
/// Returns an error if the DB update fails. Silently succeeds if no row matches `id`
/// (including one that has since been soft-deleted).
pub async fn mark_interrupted(db: &DbHandle, id: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE work_items
         SET status = 'interrupted', updated_at = ?
         WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Soft-delete a work item. C10-guarded (`WHERE id = ? AND deleted_at IS NULL`) so a
/// double-delete or a race with an internal status update is detected via
/// `rows_affected()` rather than a separate SELECT-then-UPDATE.
///
/// Returns `true` if a row was actually deleted, `false` if `id` did not match an
/// active row (already deleted, or never existed).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn soft_delete(db: &DbHandle, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE work_items SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// List all non-terminal, non-deleted work items (queued, running, paused, interrupted).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_active(db: &DbHandle) -> Result<Vec<WorkItem>> {
    Ok(sqlx::query_as::<_, WorkItem>(
        "SELECT * FROM work_items
         WHERE status IN ('queued', 'running', 'paused', 'interrupted')
           AND deleted_at IS NULL
         ORDER BY created_at ASC",
    )
    .fetch_all(db.pool())
    .await?)
}

/// List only interrupted, non-deleted items (shown to user on startup).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_interrupted(db: &DbHandle) -> Result<Vec<WorkItem>> {
    Ok(sqlx::query_as::<_, WorkItem>(
        "SELECT * FROM work_items
         WHERE status = 'interrupted' AND deleted_at IS NULL
         ORDER BY created_at ASC",
    )
    .fetch_all(db.pool())
    .await?)
}

/// Get a single non-deleted work item by id.
///
/// Returns `None` if no active item with the given id exists.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get(db: &DbHandle, id: &str) -> Result<Option<WorkItem>> {
    Ok(sqlx::query_as::<_, WorkItem>(
        "SELECT * FROM work_items WHERE id = ? AND deleted_at IS NULL",
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
         WHERE status = 'running' AND deleted_at IS NULL",
    )
    .bind(&now)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::sessions;

    async fn db() -> (tempfile::TempDir, DbHandle, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        let session_id = Uuid::new_v4().to_string();
        sessions::create_session(&db, &session_id, "test", None)
            .await
            .unwrap();
        (dir, db, session_id)
    }

    #[tokio::test]
    async fn link_plan_sets_path_and_reads_back_on_the_row() {
        let (_dir, db, session_id) = db().await;
        let item = create(&db, &session_id, "build the thing").await.unwrap();
        assert!(item.plan_path.is_none(), "a fresh item has no linked plan");

        let linked = link_plan(&db, &item.id, ".agents/260707-plan/plan.md")
            .await
            .unwrap();
        assert!(linked, "linking an active item must report a row updated");

        let after = get(&db, &item.id).await.unwrap().expect("row");
        assert_eq!(after.plan_path.as_deref(), Some(".agents/260707-plan/plan.md"));
    }

    #[tokio::test]
    async fn link_plan_reports_false_for_a_missing_item() {
        let (_dir, db, _session_id) = db().await;
        let linked = link_plan(&db, "does-not-exist", ".agents/x/plan.md")
            .await
            .unwrap();
        assert!(!linked, "linking a non-existent item must report no row updated");
    }
}
