//! View Engine Phase A telemetry (phase 3) — the Phase-B gate signal (design §14): a
//! `presented`/`viewed`/`projection_switched`/`usefulness`/`edit_demand` funnel over
//! `view_events`. See migration 0033 for the `kind`/`detail` contract.
//!
//! Never on a hot path and never journaled — this is observability only (no approval, no
//! risk gating; see the phase file's Requirements #5). Callers must never propagate an
//! insert failure with `?` on a path a view/telemetry write shares with a user-facing action
//! (mirrors `routing_decisions`' write contract): log and continue instead.

use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

/// One `view_events` row.
#[derive(Debug, Clone, FromRow)]
pub struct ViewEvent {
    pub id: String,
    pub session_id: String,
    pub view_id: String,
    /// 'presented' | 'viewed' | 'projection_switched' | 'usefulness' | 'edit_demand'.
    pub kind: String,
    pub detail: Option<String>,
    pub created_at: String,
}

/// Insert one telemetry row, EXCEPT `kind == "edit_demand"` with an empty/missing `detail` —
/// that combination is silently dropped (returns `Ok(None)`, no row written), the funnel's
/// explicit anti-false-positive design: "a click alone is not demand" (design §14). Every
/// other `kind`/`detail` combination always inserts and returns `Ok(Some(_))`.
///
/// # Errors
/// Returns an error only if the insert itself fails (a dropped edit-demand is not an error).
pub async fn insert_view_event(
    db: &DbHandle,
    kind: &str,
    view_id: &str,
    session_id: &str,
    detail: Option<&str>,
) -> Result<Option<ViewEvent>> {
    let empty_edit_demand =
        kind == "edit_demand" && detail.map(str::trim).map(str::is_empty).unwrap_or(true);
    if empty_edit_demand {
        return Ok(None);
    }

    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let row = sqlx::query_as::<_, ViewEvent>(
        "INSERT INTO view_events (id, session_id, view_id, kind, detail, created_at)
         VALUES (?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(session_id)
    .bind(view_id)
    .bind(kind)
    .bind(detail)
    .bind(&now)
    .fetch_one(db.pool())
    .await?;
    Ok(Some(row))
}

/// Count of `view_events` rows of a given `kind` — the funnel's raw numerator/denominator
/// inputs (e.g. `count_by_kind(db, "presented")` is `views_presented`).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn count_by_kind(db: &DbHandle, kind: &str) -> Result<i64> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM view_events WHERE kind = ?")
        .bind(kind)
        .fetch_one(db.pool())
        .await?;
    Ok(count)
}

/// Distinct sessions with at least one `view_events` row — the funnel's `active_sessions`
/// denominator.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn count_active_sessions(db: &DbHandle) -> Result<i64> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(DISTINCT session_id) FROM view_events")
        .fetch_one(db.pool())
        .await?;
    Ok(count)
}

/// The edit-demand GO-ratio funnel's raw inputs (design §14 / Phase-B gate signal). Ratios
/// are computed by [`Self::demand_per_view`]/[`Self::demand_per_session`] rather than in SQL
/// so an empty funnel (denominator 0) reads as "no signal yet", not a division error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditDemandFunnel {
    pub edit_demand_with_intent: i64,
    pub views_presented: i64,
    pub active_sessions: i64,
}

impl EditDemandFunnel {
    /// `edit_demand_with_intent / views_presented`, or `None` when no view has been
    /// presented yet.
    pub fn demand_per_view(&self) -> Option<f64> {
        (self.views_presented > 0)
            .then(|| self.edit_demand_with_intent as f64 / self.views_presented as f64)
    }

    /// `edit_demand_with_intent / active_sessions`, or `None` when no session has produced a
    /// view event yet.
    pub fn demand_per_session(&self) -> Option<f64> {
        (self.active_sessions > 0)
            .then(|| self.edit_demand_with_intent as f64 / self.active_sessions as f64)
    }
}

/// Compute the current [`EditDemandFunnel`] snapshot.
///
/// # Errors
/// Returns an error if any of the underlying count queries fail.
pub async fn edit_demand_funnel(db: &DbHandle) -> Result<EditDemandFunnel> {
    Ok(EditDemandFunnel {
        edit_demand_with_intent: count_by_kind(db, "edit_demand").await?,
        views_presented: count_by_kind(db, "presented").await?,
        active_sessions: count_active_sessions(db).await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (tempfile::TempDir, DbHandle) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn presented_view_writes_one_row() {
        let (_dir, db) = db().await;
        let view_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        let row = insert_view_event(&db, "presented", &view_id, &session_id, None)
            .await
            .unwrap()
            .expect("a non-edit_demand insert always returns Some");
        assert_eq!(row.kind, "presented");
        assert_eq!(row.view_id, view_id);
        assert_eq!(row.session_id, session_id);
        assert!(row.detail.is_none());
        assert_eq!(count_by_kind(&db, "presented").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn projection_switched_and_usefulness_carry_detail() {
        let (_dir, db) = db().await;
        let view_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();

        let switched = insert_view_event(
            &db,
            "projection_switched",
            &view_id,
            &session_id,
            Some("cards"),
        )
        .await
        .unwrap()
        .expect("insert returns Some");
        assert_eq!(switched.detail.as_deref(), Some("cards"));

        let thumb = insert_view_event(&db, "usefulness", &view_id, &session_id, Some("up"))
            .await
            .unwrap()
            .expect("insert returns Some");
        assert_eq!(thumb.detail.as_deref(), Some("up"));
    }

    #[tokio::test]
    async fn edit_demand_with_empty_or_missing_detail_inserts_nothing() {
        let (_dir, db) = db().await;
        let view_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();

        assert!(
            insert_view_event(&db, "edit_demand", &view_id, &session_id, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            insert_view_event(&db, "edit_demand", &view_id, &session_id, Some(""))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            insert_view_event(&db, "edit_demand", &view_id, &session_id, Some("   "))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            count_by_kind(&db, "edit_demand").await.unwrap(),
            0,
            "a click alone must never count as demand"
        );
    }

    #[tokio::test]
    async fn edit_demand_with_non_empty_detail_inserts_correctly() {
        let (_dir, db) = db().await;
        let view_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();

        let row = insert_view_event(
            &db,
            "edit_demand",
            &view_id,
            &session_id,
            Some("let me rename this column"),
        )
        .await
        .unwrap()
        .expect("non-empty detail must insert");
        assert_eq!(row.detail.as_deref(), Some("let me rename this column"));
        assert_eq!(count_by_kind(&db, "edit_demand").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn edit_demand_funnel_computes_ratios_and_handles_empty_denominators() {
        let (_dir, db) = db().await;
        let empty = edit_demand_funnel(&db).await.unwrap();
        assert_eq!(
            empty.demand_per_view(),
            None,
            "no views presented yet — no ratio"
        );
        assert_eq!(
            empty.demand_per_session(),
            None,
            "no sessions yet — no ratio"
        );

        let s1 = Uuid::new_v4().to_string();
        let s2 = Uuid::new_v4().to_string();
        let v1 = Uuid::new_v4().to_string();
        let v2 = Uuid::new_v4().to_string();
        insert_view_event(&db, "presented", &v1, &s1, None)
            .await
            .unwrap();
        insert_view_event(&db, "presented", &v2, &s2, None)
            .await
            .unwrap();
        insert_view_event(&db, "edit_demand", &v1, &s1, Some("rename field"))
            .await
            .unwrap();

        let funnel = edit_demand_funnel(&db).await.unwrap();
        assert_eq!(funnel.views_presented, 2);
        assert_eq!(funnel.edit_demand_with_intent, 1);
        assert_eq!(funnel.active_sessions, 2);
        assert_eq!(funnel.demand_per_view(), Some(0.5));
        assert_eq!(funnel.demand_per_session(), Some(0.5));
    }
}
