use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub content: String,
    pub tags: Option<String>,
    pub wikilinks: Option<String>,
    pub domain_id: Option<String>,
    pub embedding: Option<Vec<u8>>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

pub async fn insert(
    db: &DbHandle,
    title: &str,
    content: &str,
    tags: Option<&str>,
    domain_id: Option<&str>,
    embedding: Option<&[u8]>,
) -> Result<Note> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, Note>(
        "INSERT INTO notes
             (id, title, content, tags, domain_id, embedding, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(title)
    .bind(content)
    .bind(tags)
    .bind(domain_id)
    .bind(embedding)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

pub async fn get(db: &DbHandle, id: &str) -> Result<Option<Note>> {
    Ok(
        sqlx::query_as::<_, Note>("SELECT * FROM notes WHERE id = ? AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(db.pool())
            .await?,
    )
}

/// FTS5 BM25 full-text search on title + content.
pub async fn search_fts(db: &DbHandle, query: &str, limit: i64) -> Result<Vec<Note>> {
    Ok(sqlx::query_as::<_, Note>(
        "SELECT n.* FROM notes n
         JOIN notes_fts ON n.rowid = notes_fts.rowid
         WHERE notes_fts MATCH ?
           AND n.deleted_at IS NULL
         ORDER BY rank LIMIT ?",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(db.pool())
    .await?)
}

pub async fn soft_delete(db: &DbHandle, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE notes SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}

pub async fn set_wikilinks(db: &DbHandle, id: &str, wikilinks: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE notes SET wikilinks = ?, updated_at = ? WHERE id = ?")
        .bind(wikilinks)
        .bind(&now)
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn update_content(db: &DbHandle, id: &str, title: &str, content: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE notes SET title = ?, content = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(title)
    .bind(content)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}
