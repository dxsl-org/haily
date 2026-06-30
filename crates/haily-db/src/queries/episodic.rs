use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct EpisodicEntry {
    pub id: String,
    pub session_id: String,
    pub summary: String,
    pub key_topics: Option<String>,
    pub embedding: Option<Vec<u8>>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

pub async fn insert(
    db: &DbHandle,
    session_id: &str,
    summary: &str,
    key_topics: Option<&str>,
    embedding: Option<&[u8]>,
) -> Result<EpisodicEntry> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, EpisodicEntry>(
        "INSERT INTO kms_episodic
             (id, session_id, summary, key_topics, embedding, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(session_id)
    .bind(summary)
    .bind(key_topics)
    .bind(embedding)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// Most recent N episodic summaries across all sessions (for proactive context).
pub async fn recent(db: &DbHandle, limit: i64) -> Result<Vec<EpisodicEntry>> {
    Ok(sqlx::query_as::<_, EpisodicEntry>(
        "SELECT * FROM kms_episodic
         WHERE deleted_at IS NULL
         ORDER BY created_at DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db.pool())
    .await?)
}

pub async fn for_session(db: &DbHandle, session_id: &str) -> Result<Vec<EpisodicEntry>> {
    Ok(sqlx::query_as::<_, EpisodicEntry>(
        "SELECT * FROM kms_episodic
         WHERE session_id = ? AND deleted_at IS NULL
         ORDER BY created_at ASC",
    )
    .bind(session_id)
    .fetch_all(db.pool())
    .await?)
}
