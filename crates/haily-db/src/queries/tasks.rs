use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub priority: String,
    pub status: String,
    pub due_at: Option<String>,
    pub completed_at: Option<String>,
    pub calendar_event_id: Option<String>,
    pub domain_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

pub async fn insert(
    db: &DbHandle,
    title: &str,
    description: Option<&str>,
    priority: &str,
    due_at: Option<&str>,
    domain_id: Option<&str>,
) -> Result<Task> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, Task>(
        "INSERT INTO tasks
             (id, title, description, priority, status, due_at, domain_id, created_at, updated_at)
         VALUES (?, ?, ?, ?, 'todo', ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(title)
    .bind(description)
    .bind(priority)
    .bind(due_at)
    .bind(domain_id)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// Active tasks not done or cancelled, ordered by priority then due date.
pub async fn active(db: &DbHandle) -> Result<Vec<Task>> {
    Ok(sqlx::query_as::<_, Task>(
        "SELECT * FROM tasks
         WHERE status NOT IN ('done', 'cancelled') AND deleted_at IS NULL
         ORDER BY
             CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1
                           WHEN 'medium' THEN 2 ELSE 3 END,
             due_at ASC NULLS LAST",
    )
    .fetch_all(db.pool())
    .await?)
}

pub async fn update_status(db: &DbHandle, id: &str, status: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let completed_at = if status == "done" { Some(now.clone()) } else { None };
    sqlx::query(
        "UPDATE tasks
         SET status = ?, completed_at = ?, updated_at = ?
         WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(status)
    .bind(completed_at)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

pub async fn soft_delete(db: &DbHandle, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE tasks SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL"
    )
    .bind(&now).bind(&now).bind(id)
    .execute(db.pool()).await?.rows_affected();
    Ok(rows > 0)
}

/// FTS5 BM25 search on title + description.
pub async fn search_fts(db: &DbHandle, query: &str, limit: i64) -> Result<Vec<Task>> {
    Ok(sqlx::query_as::<_, Task>(
        "SELECT t.* FROM tasks t
         JOIN tasks_fts ON t.rowid = tasks_fts.rowid
         WHERE tasks_fts MATCH ?
           AND t.deleted_at IS NULL
         ORDER BY rank LIMIT ?",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(db.pool())
    .await?)
}
