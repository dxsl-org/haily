//! Version history for the skill editor (Unified Chat UI phase 8, D4) — one append-only table
//! shared by both authored (kit-pack file) and synthesized (`kms_skills` row) skills. A row is
//! inserted BEFORE every mutation with the content that was live immediately before it, which
//! doubles as both the revert history and the crash-safety net for the authored atomic write
//! (see `haily_kms::skill_editor::ops::snapshot_current` and `haily_kms::kit_pack`'s
//! boot-fallback loader).

use crate::DbHandle;
use anyhow::Result;
use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

/// One saved state of a skill. `content_md` is the full authored file (frontmatter + body) when
/// `kind == "authored"`, or the rendered 4-section body when `kind == "synthesized"` — see the
/// migration's doc comment for why the shape differs by kind.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct SkillVersion {
    pub id: String,
    pub skill_name: String,
    pub kind: String,
    pub content_md: String,
    pub sha256: String,
    pub note: Option<String>,
    pub created_at: String,
}

/// Record a version.
///
/// # Errors
/// Returns an error if the insert fails.
pub async fn insert_version(
    db: &DbHandle,
    skill_name: &str,
    kind: &str,
    content_md: &str,
    sha256: &str,
    note: Option<&str>,
) -> Result<SkillVersion> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, SkillVersion>(
        "INSERT INTO skill_versions (id, skill_name, kind, content_md, sha256, note, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(skill_name)
    .bind(kind)
    .bind(content_md)
    .bind(sha256)
    .bind(note)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// All versions of `skill_name`, newest first.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_versions(db: &DbHandle, skill_name: &str) -> Result<Vec<SkillVersion>> {
    Ok(sqlx::query_as::<_, SkillVersion>(
        "SELECT * FROM skill_versions WHERE skill_name = ? ORDER BY created_at DESC",
    )
    .bind(skill_name)
    .fetch_all(db.pool())
    .await?)
}

/// Fetch one version by id — the `revert_skill` lookup key.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get_version(db: &DbHandle, id: &str) -> Result<Option<SkillVersion>> {
    Ok(
        sqlx::query_as::<_, SkillVersion>("SELECT * FROM skill_versions WHERE id = ?")
            .bind(id)
            .fetch_optional(db.pool())
            .await?,
    )
}

/// The newest version recorded for `skill_name`, if any — the boot-fallback lookup key
/// (`haily_kms::kit_pack::load_with_versions_fallback`).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn latest_version(db: &DbHandle, skill_name: &str) -> Result<Option<SkillVersion>> {
    Ok(sqlx::query_as::<_, SkillVersion>(
        "SELECT * FROM skill_versions WHERE skill_name = ? ORDER BY created_at DESC LIMIT 1",
    )
    .bind(skill_name)
    .fetch_optional(db.pool())
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> (DbHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn insert_then_list_returns_newest_first() {
        let (db, _dir) = test_db().await;
        insert_version(&db, "plan", "authored", "v1", "hash1", None).await.unwrap();
        insert_version(&db, "plan", "authored", "v2", "hash2", Some("edit"))
            .await
            .unwrap();

        let versions = list_versions(&db, "plan").await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].content_md, "v2", "newest first");
        assert_eq!(versions[1].content_md, "v1");
    }

    #[tokio::test]
    async fn get_version_and_latest_version_round_trip() {
        let (db, _dir) = test_db().await;
        let v1 = insert_version(&db, "cook", "synthesized", "body1", "h1", None)
            .await
            .unwrap();
        let v2 = insert_version(&db, "cook", "synthesized", "body2", "h2", None)
            .await
            .unwrap();

        let fetched = get_version(&db, &v1.id).await.unwrap().unwrap();
        assert_eq!(fetched.content_md, "body1");

        let latest = latest_version(&db, "cook").await.unwrap().unwrap();
        assert_eq!(latest.id, v2.id);

        assert!(get_version(&db, "does-not-exist").await.unwrap().is_none());
        assert!(latest_version(&db, "no-such-skill").await.unwrap().is_none());
    }
}
