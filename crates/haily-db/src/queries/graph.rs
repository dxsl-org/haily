use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Entity {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct EntityEdge {
    pub id: String,
    pub from_id: String,
    pub to_id: String,
    pub predicate: String,
    pub fact_id: Option<String>,
    pub weight: f64,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

/// Insert or update an entity by name (upsert on name uniqueness).
pub async fn upsert_entity(db: &DbHandle, name: &str, entity_type: &str) -> Result<Entity> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, Entity>(
        "INSERT INTO entities (id, name, entity_type, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(name) DO UPDATE SET
             entity_type = excluded.entity_type,
             updated_at  = excluded.updated_at
         RETURNING *",
    )
    .bind(&id)
    .bind(name)
    .bind(entity_type)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

pub async fn upsert_edge(
    db: &DbHandle,
    from_id: &str,
    to_id: &str,
    predicate: &str,
    fact_id: Option<&str>,
) -> Result<EntityEdge> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, EntityEdge>(
        "INSERT INTO entity_edges (id, from_id, to_id, predicate, fact_id, weight, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 1.0, ?, ?)
         ON CONFLICT(from_id, to_id, predicate) DO UPDATE SET
             weight     = MIN(weight + 0.1, 5.0),
             updated_at = excluded.updated_at
         RETURNING *",
    )
    .bind(&id)
    .bind(from_id)
    .bind(to_id)
    .bind(predicate)
    .bind(fact_id)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// One-hop neighbors of an entity — for knowledge graph context building.
pub async fn neighbors(db: &DbHandle, entity_id: &str) -> Result<Vec<EntityEdge>> {
    Ok(sqlx::query_as::<_, EntityEdge>(
        "SELECT * FROM entity_edges
         WHERE (from_id = ? OR to_id = ?) AND deleted_at IS NULL
         ORDER BY weight DESC LIMIT 50",
    )
    .bind(entity_id)
    .bind(entity_id)
    .fetch_all(db.pool())
    .await?)
}
