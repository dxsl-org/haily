//! Routing decision log (Auto Model Routing R1, phase 2) — the R2 training set.
//!
//! One row per unit of work (a chat turn or a pipeline stage), written best-effort at unit
//! END by the caller (Phases 4/6). This module never enforces the write timing itself — it
//! only persists what it is given — but the schema's write contract (see migration 0031) is:
//! never on the pre-first-token hot path, never with `?` (a telemetry write must not abort a
//! chat turn), and a crashed unit writes nothing.
//!
//! Only derived features are stored — never raw message text — both for privacy and because
//! this table is read back by future model calls (prompt-injection surface).

use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

/// One routing decision row. Mirrors the `eval_runs` / `pipeline_runs` idiom: a `FromRow`
/// struct, RFC3339 timestamps, fully parameterized SQL kept in this leaf crate.
#[derive(Debug, Clone, FromRow)]
pub struct RoutingDecision {
    pub id: String,
    pub turn_id: String,
    pub run_id: Option<String>,
    /// 'chat' | 'pipeline_stage'.
    pub context_kind: String,
    pub stage_kind: Option<String>,
    /// 'fast' | 'medium' | 'thinking' | 'ultra' | NULL (session default).
    pub chosen_tier: Option<String>,
    /// Tier the unit escalated to mid-flight; `None` = no escalation happened.
    pub escalated_to: Option<String>,
    /// 'default' | 'heuristic' | 'explicit_phrase' | 'depth'.
    pub decision_source: String,
    pub cost_quality: i64,
    pub feature_msg_words: i64,
    pub feature_has_code: bool,
    /// Count of PRIOR USER messages in context (trusted-origin signal only).
    pub feature_history_user_msgs: i64,
    /// `DepthMode::as_label` ('quick' | 'normal' | 'deep').
    pub feature_depth: String,
    /// `None` | 'stream_init_error' | 'gate_failure'.
    pub escalation_trigger: Option<String>,
    pub prior_failures: i64,
    pub created_at: String,
}

/// Every field a routing-decision insert carries, grouped so [`insert`] stays within a sane
/// arity (the `journal::NewAction` / `eval_runs::NewEvalRun` convention).
pub struct NewRoutingDecision<'a> {
    pub turn_id: &'a str,
    pub run_id: Option<&'a str>,
    /// 'chat' | 'pipeline_stage'.
    pub context_kind: &'a str,
    pub stage_kind: Option<&'a str>,
    pub chosen_tier: Option<&'a str>,
    pub escalated_to: Option<&'a str>,
    pub decision_source: &'a str,
    pub cost_quality: i64,
    pub feature_msg_words: i64,
    pub feature_has_code: bool,
    pub feature_history_user_msgs: i64,
    pub feature_depth: &'a str,
    pub escalation_trigger: Option<&'a str>,
    pub prior_failures: i64,
}

/// Persist one routing decision, returning the inserted row.
///
/// # Errors
/// Returns an error if the insert fails. Per the write contract (migration 0031), callers
/// must treat this as best-effort telemetry and never propagate the error with `?` on a
/// hot chat/pipeline path — log and continue instead.
pub async fn insert(db: &DbHandle, new: NewRoutingDecision<'_>) -> Result<RoutingDecision> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, RoutingDecision>(
        "INSERT INTO routing_decisions
             (id, turn_id, run_id, context_kind, stage_kind, chosen_tier, escalated_to,
              decision_source, cost_quality, feature_msg_words, feature_has_code,
              feature_history_user_msgs, feature_depth, escalation_trigger, prior_failures,
              created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(new.turn_id)
    .bind(new.run_id)
    .bind(new.context_kind)
    .bind(new.stage_kind)
    .bind(new.chosen_tier)
    .bind(new.escalated_to)
    .bind(new.decision_source)
    .bind(new.cost_quality)
    .bind(new.feature_msg_words)
    .bind(new.feature_has_code)
    .bind(new.feature_history_user_msgs)
    .bind(new.feature_depth)
    .bind(new.escalation_trigger)
    .bind(new.prior_failures)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// Most recent routing decisions across all turns, newest first.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_recent(db: &DbHandle, limit: i64) -> Result<Vec<RoutingDecision>> {
    Ok(sqlx::query_as::<_, RoutingDecision>(
        "SELECT * FROM routing_decisions ORDER BY created_at DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db.pool())
    .await?)
}

/// All routing decisions sharing a `turn_id` (a turn plus any pipeline stages it spawned),
/// oldest first so the sequence reads in decision order.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_by_turn(db: &DbHandle, turn_id: &str) -> Result<Vec<RoutingDecision>> {
    Ok(sqlx::query_as::<_, RoutingDecision>(
        "SELECT * FROM routing_decisions WHERE turn_id = ? ORDER BY created_at ASC",
    )
    .bind(turn_id)
    .fetch_all(db.pool())
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (tempfile::TempDir, DbHandle) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        (dir, db)
    }

    fn sample(turn_id: &str) -> NewRoutingDecision<'_> {
        NewRoutingDecision {
            turn_id,
            run_id: None,
            context_kind: "chat",
            stage_kind: None,
            chosen_tier: Some("fast"),
            escalated_to: None,
            decision_source: "heuristic",
            cost_quality: 3,
            feature_msg_words: 12,
            feature_has_code: false,
            feature_history_user_msgs: 4,
            feature_depth: "normal",
            escalation_trigger: None,
            prior_failures: 0,
        }
    }

    #[tokio::test]
    async fn insert_list_by_turn_roundtrips_every_field() {
        let (_dir, db) = db().await;
        let row = insert(&db, sample("turn-1")).await.unwrap();
        assert_eq!(row.turn_id, "turn-1");
        assert_eq!(row.context_kind, "chat");
        assert_eq!(row.chosen_tier.as_deref(), Some("fast"));
        assert_eq!(row.decision_source, "heuristic");
        assert_eq!(row.cost_quality, 3);
        assert_eq!(row.feature_msg_words, 12);
        assert!(!row.feature_has_code);
        assert_eq!(row.feature_history_user_msgs, 4);
        assert_eq!(row.feature_depth, "normal");
        assert!(row.escalated_to.is_none());
        assert!(row.escalation_trigger.is_none());
        assert_eq!(row.prior_failures, 0);

        let fetched = list_by_turn(&db, "turn-1").await.unwrap();
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].id, row.id);
    }

    #[tokio::test]
    async fn list_by_turn_filters_and_orders_oldest_first() {
        let (_dir, db) = db().await;
        insert(&db, sample("turn-a")).await.unwrap();
        let mut stage = sample("turn-a");
        stage.context_kind = "pipeline_stage";
        stage.stage_kind = Some("build");
        stage.run_id = Some("run-1");
        insert(&db, stage).await.unwrap();
        insert(&db, sample("turn-b")).await.unwrap();

        let rows = list_by_turn(&db, "turn-a").await.unwrap();
        assert_eq!(rows.len(), 2, "only turn-a rows");
        assert_eq!(rows[0].context_kind, "chat");
        assert_eq!(rows[1].context_kind, "pipeline_stage");
        assert_eq!(rows[1].stage_kind.as_deref(), Some("build"));
        assert_eq!(rows[1].run_id.as_deref(), Some("run-1"));
    }

    #[tokio::test]
    async fn list_recent_respects_limit_and_orders_newest_first() {
        let (_dir, db) = db().await;
        insert(&db, sample("t1")).await.unwrap();
        insert(&db, sample("t2")).await.unwrap();
        insert(&db, sample("t3")).await.unwrap();

        let rows = list_recent(&db, 2).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].turn_id, "t3");
        assert_eq!(rows[1].turn_id, "t2");
    }

    #[tokio::test]
    async fn escalation_fields_persist_when_present() {
        let (_dir, db) = db().await;
        let mut new = sample("turn-esc");
        new.chosen_tier = Some("fast");
        new.escalated_to = Some("thinking");
        new.escalation_trigger = Some("gate_failure");
        new.prior_failures = 2;
        let row = insert(&db, new).await.unwrap();

        assert_eq!(row.escalated_to.as_deref(), Some("thinking"));
        assert_eq!(row.escalation_trigger.as_deref(), Some("gate_failure"));
        assert_eq!(row.prior_failures, 2);
    }
}
