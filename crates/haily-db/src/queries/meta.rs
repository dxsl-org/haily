use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Preference {
    pub id: String,
    pub key: String,
    pub value: String,
    pub confidence: f64,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct Feedback {
    pub id: String,
    pub session_id: String,
    pub message_id: Option<String>,
    pub reaction: String,
    pub content: Option<String>,
    pub affected_fact_id: Option<String>,
    pub created_at: String,
}

pub async fn get_preference(db: &DbHandle, key: &str) -> Result<Option<String>> {
    let row = sqlx::query_as::<_, (String,)>("SELECT value FROM kms_preferences WHERE key = ?")
        .bind(key)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.map(|(v,)| v))
}

pub async fn upsert_preference(db: &DbHandle, key: &str, value: &str, source: &str) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO kms_preferences (id, key, value, confidence, source, created_at, updated_at)
         VALUES (?, ?, ?, 1.0, ?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET
             value      = excluded.value,
             source     = excluded.source,
             updated_at = excluded.updated_at",
    )
    .bind(&id)
    .bind(key)
    .bind(value)
    .bind(source)
    .bind(&now)
    .bind(&now)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Deletes a preference row by key. Used by the scheduled backup worker to scrub a
/// plaintext-holding credential row out of a standalone backup COPY (never the live
/// database) when boot-time credential-migration status is not clean, so no plaintext
/// secret ships in a backup file (M7b, see `haily-proactive::backup::credential_scrub`).
/// A no-op (not an error) if `key` is already absent.
pub async fn delete_preference(db: &DbHandle, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM kms_preferences WHERE key = ?")
        .bind(key)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn all_preferences(db: &DbHandle) -> Result<Vec<Preference>> {
    Ok(
        sqlx::query_as::<_, Preference>("SELECT * FROM kms_preferences ORDER BY key")
            .fetch_all(db.pool())
            .await?,
    )
}

/// All preferences whose key starts with `prefix` — used for namespaced reads (e.g. "feedback.correction.").
pub async fn list_by_prefix(db: &DbHandle, prefix: &str) -> Result<Vec<Preference>> {
    Ok(sqlx::query_as::<_, Preference>(
        "SELECT * FROM kms_preferences WHERE key LIKE ? ORDER BY key",
    )
    .bind(format!("{prefix}%"))
    .fetch_all(db.pool())
    .await?)
}

pub async fn insert_feedback(
    db: &DbHandle,
    session_id: &str,
    message_id: Option<&str>,
    reaction: &str,
    content: Option<&str>,
    affected_fact_id: Option<&str>,
) -> Result<Feedback> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, Feedback>(
        "INSERT INTO kms_feedback
             (id, session_id, message_id, reaction, content, affected_fact_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(session_id)
    .bind(message_id)
    .bind(reaction)
    .bind(content)
    .bind(affected_fact_id)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}
