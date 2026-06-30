use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Fact {
    pub id: String,
    pub domain_id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f64,
    pub source: String,
    pub source_ref: Option<String>,
    pub embedding: Option<Vec<u8>>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
    pub archived_at: Option<String>,
}

pub async fn insert_fact(
    db: &DbHandle,
    domain_id: &str,
    subject: &str,
    predicate: &str,
    object: &str,
    source: &str,
    source_ref: Option<&str>,
    embedding: Option<&[u8]>,
) -> Result<Fact> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, Fact>(
        "INSERT INTO kms_facts (id, domain_id, subject, predicate, object,
             confidence, source, source_ref, embedding, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 1.0, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(domain_id)
    .bind(subject)
    .bind(predicate)
    .bind(object)
    .bind(source)
    .bind(source_ref)
    .bind(embedding)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

pub async fn get_fact(db: &DbHandle, id: &str) -> Result<Option<Fact>> {
    Ok(sqlx::query_as::<_, Fact>(
        "SELECT * FROM kms_facts WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await?)
}

/// FTS5 BM25 search. Returns most relevant facts first.
pub async fn search_fts(db: &DbHandle, query: &str, limit: i64) -> Result<Vec<Fact>> {
    Ok(sqlx::query_as::<_, Fact>(
        "SELECT f.* FROM kms_facts f
         JOIN facts_fts ON f.rowid = facts_fts.rowid
         WHERE facts_fts MATCH ?
           AND f.deleted_at IS NULL
           AND f.archived_at IS NULL
         ORDER BY rank LIMIT ?",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(db.pool())
    .await?)
}

/// Returns (id, embedding_blob) for all facts with embeddings — used at startup
/// to rebuild the in-memory HNSW index in haily-kms.
pub async fn embeddings_for_hnsw(db: &DbHandle) -> Result<Vec<(String, Vec<u8>)>> {
    let rows = sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT id, embedding FROM kms_facts
         WHERE embedding IS NOT NULL AND deleted_at IS NULL AND archived_at IS NULL",
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows)
}

pub async fn list_by_domain(db: &DbHandle, domain_id: &str, limit: i64) -> Result<Vec<Fact>> {
    Ok(sqlx::query_as::<_, Fact>(
        "SELECT * FROM kms_facts WHERE domain_id = ? AND deleted_at IS NULL AND archived_at IS NULL ORDER BY created_at DESC LIMIT ?"
    )
    .bind(domain_id).bind(limit)
    .fetch_all(db.pool()).await?)
}

pub async fn list_top(db: &DbHandle, limit: i64) -> Result<Vec<Fact>> {
    Ok(sqlx::query_as::<_, Fact>(
        "SELECT * FROM kms_facts WHERE deleted_at IS NULL AND archived_at IS NULL ORDER BY confidence DESC LIMIT ?"
    )
    .bind(limit)
    .fetch_all(db.pool()).await?)
}

pub async fn soft_delete(db: &DbHandle, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE kms_facts SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL"
    )
    .bind(&now).bind(&now).bind(id)
    .execute(db.pool()).await?.rows_affected();
    Ok(rows > 0)
}

/// EMA update: confidence = 0.8 * old + 0.2 * new_signal.
/// Archives the fact if confidence drops below 0.25.
pub async fn update_confidence(db: &DbHandle, id: &str, new_signal: f64) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE kms_facts
         SET confidence  = ROUND(0.8 * confidence + 0.2 * ?, 4),
             archived_at = CASE WHEN (0.8 * confidence + 0.2 * ?) < 0.25
                                THEN ? ELSE archived_at END,
             updated_at  = ?
         WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(new_signal)
    .bind(new_signal)
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}
