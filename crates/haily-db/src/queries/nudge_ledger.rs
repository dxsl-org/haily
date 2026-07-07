use crate::DbHandle;
use anyhow::Result;

/// Attempt to claim a cross-domain nudge firing for `(condition, entity_id, fired_on)`.
///
/// Returns `Ok(true)` when this call newly claimed the slot (the caller should fire the
/// nudge) and `Ok(false)` when it was already claimed today (the caller must suppress —
/// this is the cooldown). The claim is a single atomic `INSERT OR IGNORE` rather than a
/// separate check-then-insert: two overlapping ticks (or a tick racing a restart) can
/// never both observe "not yet fired" and both send the nudge.
///
/// The claim is permanent for `fired_on` — there is no expiry/decay path. Cooldown here
/// means "at most once per calendar day per condition per entity", not a rolling window.
pub async fn try_claim(
    db: &DbHandle,
    condition: &str,
    entity_id: &str,
    fired_on: &str,
) -> Result<bool> {
    let fired_at = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "INSERT OR IGNORE INTO nudge_cooldown_ledger (condition, entity_id, fired_on, fired_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(condition)
    .bind(entity_id)
    .bind(fired_on)
    .bind(&fired_at)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
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
    async fn first_claim_succeeds_second_claim_same_day_is_suppressed() {
        let (db, _d) = db().await;
        assert!(try_claim(&db, "cond_a", "entity-1", "2026-07-07").await.unwrap());
        assert!(!try_claim(&db, "cond_a", "entity-1", "2026-07-07").await.unwrap());
    }

    #[tokio::test]
    async fn distinct_condition_or_entity_or_day_claims_independently() {
        let (db, _d) = db().await;
        assert!(try_claim(&db, "cond_a", "entity-1", "2026-07-07").await.unwrap());
        assert!(try_claim(&db, "cond_b", "entity-1", "2026-07-07").await.unwrap());
        assert!(try_claim(&db, "cond_a", "entity-2", "2026-07-07").await.unwrap());
        assert!(try_claim(&db, "cond_a", "entity-1", "2026-07-08").await.unwrap());
    }

    /// Restart-survival: a fresh `DbHandle` opened against the same file (simulating a
    /// daemon restart, which used to reset the old in-process HashSet) must still see the
    /// earlier claim and continue suppressing it.
    #[tokio::test]
    async fn claim_survives_reopening_the_db_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db1 = DbHandle::init(&path).await.unwrap();
        assert!(try_claim(&db1, "cond_a", "entity-1", "2026-07-07").await.unwrap());
        drop(db1);

        let db2 = DbHandle::init(&path).await.unwrap();
        assert!(!try_claim(&db2, "cond_a", "entity-1", "2026-07-07").await.unwrap());
    }
}
