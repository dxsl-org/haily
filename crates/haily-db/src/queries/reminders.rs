use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Reminder {
    pub id: String,
    pub title: String,
    pub fire_at: String,
    pub recurrence: Option<String>,
    pub fired_at: Option<String>,
    pub outcome: Option<String>,
    pub outcome_at: Option<String>,
    pub session_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

pub async fn insert(
    db: &DbHandle,
    title: &str,
    fire_at: &str,
    recurrence: Option<&str>,
    session_id: Option<&str>,
) -> Result<Reminder> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, Reminder>(
        "INSERT INTO reminders (id, title, fire_at, recurrence, session_id, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(title)
    .bind(fire_at)
    .bind(recurrence)
    .bind(session_id)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// All reminders due at or before `now_rfc3339` that have not been fired.
/// Used by the ProactiveDaemon scheduler loop.
pub async fn pending(db: &DbHandle, now_rfc3339: &str) -> Result<Vec<Reminder>> {
    Ok(sqlx::query_as::<_, Reminder>(
        "SELECT * FROM reminders
         WHERE fire_at <= ? AND fired_at IS NULL AND deleted_at IS NULL
         ORDER BY fire_at ASC",
    )
    .bind(now_rfc3339)
    .fetch_all(db.pool())
    .await?)
}

pub async fn mark_fired(db: &DbHandle, id: &str, fired_at: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE reminders SET fired_at = ?, updated_at = ? WHERE id = ?")
        .bind(fired_at)
        .bind(&now)
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn list_all(db: &DbHandle) -> Result<Vec<Reminder>> {
    Ok(sqlx::query_as::<_, Reminder>(
        "SELECT * FROM reminders WHERE deleted_at IS NULL ORDER BY fire_at ASC",
    )
    .fetch_all(db.pool())
    .await?)
}

pub async fn soft_delete(db: &DbHandle, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE reminders SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}

pub async fn set_outcome(db: &DbHandle, id: &str, outcome: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE reminders SET outcome = ?, outcome_at = ?, updated_at = ? WHERE id = ?")
        .bind(outcome)
        .bind(&now)
        .bind(&now)
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}
