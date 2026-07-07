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

/// Atomically mark a recurring reminder fired AND insert its next occurrence in ONE SQLite
/// transaction (`BEGIN`...`COMMIT`) — without this, a crash (or any error) between the two
/// writes could leave a reminder marked `fired_at` with no successor row, silently ending
/// the series forever. `next_id` is minted by the caller (mirrors the id-minting convention
/// in `local_snapshot::local_journaled_write`) so the daemon can log the new id before the
/// row exists; `title` is likewise supplied by the caller (the row it read to fire) rather
/// than re-queried here, keeping this a single UPDATE + a single INSERT.
///
/// # Errors
/// Any failure (including a constraint violation on the INSERT) rolls back the transaction —
/// the UPDATE never lands without its paired INSERT, and vice versa.
#[allow(clippy::too_many_arguments)]
pub async fn mark_fired_and_reschedule(
    db: &DbHandle,
    id: &str,
    fired_at: &str,
    next_id: &str,
    next_fire: &str,
    rule: &str,
    title: &str,
    session_id: Option<&str>,
) -> Result<Reminder> {
    let now = chrono::Utc::now().to_rfc3339();
    let mut tx = db.pool().begin().await?;

    sqlx::query("UPDATE reminders SET fired_at = ?, updated_at = ? WHERE id = ?")
        .bind(fired_at)
        .bind(&now)
        .bind(id)
        .execute(&mut *tx)
        .await?;

    let next = sqlx::query_as::<_, Reminder>(
        "INSERT INTO reminders (id, title, fire_at, recurrence, session_id, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(next_id)
    .bind(title)
    .bind(next_fire)
    .bind(rule)
    .bind(session_id)
    .bind(&now)
    .bind(&now)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(next)
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (DbHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn mark_fired_and_reschedule_commits_both_writes_together() {
        let (db, _d) = db().await;
        let r = insert(&db, "Water plants", "2026-01-01T07:00:00Z", Some("daily"), None)
            .await
            .unwrap();

        let next = mark_fired_and_reschedule(
            &db,
            &r.id,
            "2026-01-01T07:00:05Z",
            "next-1",
            "2026-01-02T07:00:00Z",
            "daily",
            &r.title,
            None,
        )
        .await
        .unwrap();

        let fired = sqlx::query_as::<_, Reminder>("SELECT * FROM reminders WHERE id = ?")
            .bind(&r.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(fired.fired_at.as_deref(), Some("2026-01-01T07:00:05Z"));
        assert_eq!(next.fire_at, "2026-01-02T07:00:00Z");
        assert_eq!(next.recurrence.as_deref(), Some("daily"));
    }

    /// Durability contract: the UPDATE (mark fired) and the INSERT (reschedule) run in ONE
    /// transaction. Forcing the INSERT to fail (a `next_id` colliding with an existing row's
    /// PRIMARY KEY, standing in for "a crash/error between the two writes") must roll back
    /// the UPDATE too — a recurring reminder is never left "fired" with no successor.
    #[tokio::test]
    async fn mark_fired_and_reschedule_rolls_back_the_mark_when_the_reschedule_insert_fails() {
        let (db, _d) = db().await;
        let r = insert(&db, "Water plants", "2026-01-01T07:00:00Z", Some("daily"), None)
            .await
            .unwrap();
        let other = insert(&db, "Unrelated", "2026-01-01T08:00:00Z", None, None).await.unwrap();

        let result = mark_fired_and_reschedule(
            &db,
            &r.id,
            "2026-01-01T07:00:05Z",
            &other.id, // PRIMARY KEY collision forces the INSERT to fail
            "2026-01-02T07:00:00Z",
            "daily",
            &r.title,
            None,
        )
        .await;
        assert!(result.is_err(), "colliding id must fail the reschedule insert");

        let untouched = sqlx::query_as::<_, Reminder>("SELECT * FROM reminders WHERE id = ?")
            .bind(&r.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert!(
            untouched.fired_at.is_none(),
            "the UPDATE must have rolled back along with the failed INSERT — \
             a reminder must never be left fired with no successor"
        );
    }
}
