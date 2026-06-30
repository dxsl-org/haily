use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Session {
    pub id: String,
    pub adapter_id: String,
    pub user_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub tokens: Option<i64>,
    pub created_at: String,
}

pub async fn create_session(
    db: &DbHandle,
    adapter_id: &str,
    user_ref: Option<&str>,
) -> Result<Session> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, Session>(
        "INSERT INTO sessions (id, adapter_id, user_ref, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(adapter_id)
    .bind(user_ref)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

pub async fn get_session(db: &DbHandle, id: &str) -> Result<Option<Session>> {
    Ok(sqlx::query_as::<_, Session>(
        "SELECT * FROM sessions WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await?)
}

pub async fn touch_session(db: &DbHandle, id: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE sessions SET updated_at = ? WHERE id = ?")
        .bind(&now)
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn insert_message(
    db: &DbHandle,
    session_id: &str,
    role: &str,
    content: &str,
    tokens: Option<i64>,
) -> Result<Message> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, Message>(
        "INSERT INTO messages (id, session_id, role, content, tokens, created_at)
         VALUES (?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(session_id)
    .bind(role)
    .bind(content)
    .bind(tokens)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// Returns messages in chronological order (oldest first) for LLM context window.
pub async fn recent_messages(
    db: &DbHandle,
    session_id: &str,
    limit: i64,
) -> Result<Vec<Message>> {
    Ok(sqlx::query_as::<_, Message>(
        "SELECT * FROM (
             SELECT * FROM messages WHERE session_id = ?
             ORDER BY created_at DESC LIMIT ?
         ) ORDER BY created_at ASC",
    )
    .bind(session_id)
    .bind(limit)
    .fetch_all(db.pool())
    .await?)
}
