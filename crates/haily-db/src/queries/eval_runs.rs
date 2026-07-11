//! Eval-run persistence (Sub-Agent + Skill Architecture phase 9, Golden Coding Eval).
//!
//! One row per scored eval task run — the measurement schema Router A/B + the escalation-default
//! (P3) decision read from. Mirrors the `pipeline_runs`/`work_items` idiom: a `FromRow` struct,
//! RFC3339 timestamps, fully parameterized SQL kept in this leaf crate, and a `deleted_at IS
//! NULL` guard on every read. `task_kind` ('coding'|'automation') lets P14's automation eval
//! reuse this table with no new migration.

use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

/// One scored eval run. The JSON blob columns (`per_stage_tokens`, `egress`, `gate_results`)
/// are opaque here — the `haily-core` eval runner serializes/deserializes them; this layer
/// stores them verbatim (same contract as `pipeline_runs.per_attempt_tokens`).
#[derive(Debug, Clone, FromRow)]
pub struct EvalRun {
    pub id: String,
    pub task_id: String,
    /// 'coding' (P9) | 'automation' (P14).
    pub task_kind: String,
    pub model: String,
    pub tier_config: String,
    pub depth: String,
    pub per_stage_tokens: Option<String>,
    pub escalation_count: i64,
    /// Per-attempt egress tags (JSON) — FMA-M2 (a local arm that crossed to cloud is visible).
    pub egress: Option<String>,
    pub wall_clock_ms: i64,
    /// `true` iff every deterministic scoring gate passed.
    pub passed: bool,
    pub gate_results: Option<String>,
    pub created_at: String,
    pub deleted_at: Option<String>,
}

/// Every field an eval-run insert carries, grouped so [`insert`] stays within a sane arity
/// (the `journal::NewAction` / `pipeline_runs::RunTransition` idiom).
pub struct NewEvalRun<'a> {
    pub task_id: &'a str,
    /// 'coding' | 'automation'.
    pub task_kind: &'a str,
    pub model: &'a str,
    pub tier_config: &'a str,
    pub depth: &'a str,
    /// JSON array of per-stage token records (or `None` when the backend surfaces no usage).
    pub per_stage_tokens: Option<&'a str>,
    pub escalation_count: i64,
    /// JSON array of per-attempt egress tags (FMA-M2).
    pub egress: Option<&'a str>,
    pub wall_clock_ms: i64,
    pub passed: bool,
    /// JSON array of scored gate results.
    pub gate_results: Option<&'a str>,
}

/// Persist one scored eval run, returning the inserted row.
///
/// # Errors
/// Returns an error if the insert fails.
pub async fn insert(db: &DbHandle, new: NewEvalRun<'_>) -> Result<EvalRun> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, EvalRun>(
        "INSERT INTO eval_runs
             (id, task_id, task_kind, model, tier_config, depth, per_stage_tokens,
              escalation_count, egress, wall_clock_ms, passed, gate_results, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(new.task_id)
    .bind(new.task_kind)
    .bind(new.model)
    .bind(new.tier_config)
    .bind(new.depth)
    .bind(new.per_stage_tokens)
    .bind(new.escalation_count)
    .bind(new.egress)
    .bind(new.wall_clock_ms)
    .bind(new.passed)
    .bind(new.gate_results)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// Get a single non-deleted eval run by id. `None` if none exists.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get(db: &DbHandle, id: &str) -> Result<Option<EvalRun>> {
    Ok(
        sqlx::query_as::<_, EvalRun>("SELECT * FROM eval_runs WHERE id = ? AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(db.pool())
            .await?,
    )
}

/// List non-deleted eval runs for a `task_kind`, newest first (the Router A/B / escalation
/// comparison table reads this).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_by_kind(db: &DbHandle, task_kind: &str) -> Result<Vec<EvalRun>> {
    Ok(sqlx::query_as::<_, EvalRun>(
        "SELECT * FROM eval_runs
         WHERE task_kind = ? AND deleted_at IS NULL
         ORDER BY created_at DESC",
    )
    .bind(task_kind)
    .fetch_all(db.pool())
    .await?)
}

/// Soft-delete an eval run. Returns `true` if a row was actually deleted.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn soft_delete(db: &DbHandle, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows =
        sqlx::query("UPDATE eval_runs SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(&now)
            .bind(id)
            .execute(db.pool())
            .await?
            .rows_affected();
    Ok(rows > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (tempfile::TempDir, DbHandle) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        (dir, db)
    }

    fn sample(task_id: &str, passed: bool) -> NewEvalRun<'_> {
        NewEvalRun {
            task_id,
            task_kind: "coding",
            model: "test-model",
            tier_config: "local",
            depth: "normal",
            per_stage_tokens: Some(r#"[{"stage":"build","attempt":0}]"#),
            escalation_count: 1,
            egress: Some(r#"[{"attempt":0,"egress":"local"}]"#),
            wall_clock_ms: 1234,
            passed,
            gate_results: Some(r#"[{"gate":"tests","pass":true}]"#),
        }
    }

    #[tokio::test]
    async fn insert_get_roundtrips_every_field() {
        let (_dir, db) = db().await;
        let row = insert(&db, sample("rust-fix-compile", true)).await.unwrap();
        assert_eq!(row.task_id, "rust-fix-compile");
        assert_eq!(row.task_kind, "coding");
        assert!(row.passed);
        assert_eq!(row.escalation_count, 1);
        assert_eq!(row.wall_clock_ms, 1234);
        assert!(row.egress.as_deref().unwrap().contains("local"));
        assert!(row.gate_results.as_deref().unwrap().contains("tests"));

        let fetched = get(&db, &row.id).await.unwrap().expect("row");
        assert_eq!(fetched.id, row.id);
    }

    #[tokio::test]
    async fn list_by_kind_orders_newest_first_and_filters_kind() {
        let (_dir, db) = db().await;
        insert(&db, sample("a", true)).await.unwrap();
        insert(&db, sample("b", false)).await.unwrap();
        let mut auto = sample("c", true);
        auto.task_kind = "automation";
        insert(&db, auto).await.unwrap();

        let coding = list_by_kind(&db, "coding").await.unwrap();
        assert_eq!(coding.len(), 2, "only coding rows");
        assert!(coding.iter().all(|r| r.task_kind == "coding"));

        let automation = list_by_kind(&db, "automation").await.unwrap();
        assert_eq!(automation.len(), 1);
    }

    #[tokio::test]
    async fn soft_delete_hides_the_row() {
        let (_dir, db) = db().await;
        let row = insert(&db, sample("x", true)).await.unwrap();
        assert!(soft_delete(&db, &row.id).await.unwrap());
        assert!(get(&db, &row.id).await.unwrap().is_none(), "soft-deleted row is hidden");
        assert!(!soft_delete(&db, &row.id).await.unwrap(), "double-delete is a no-op");
    }
}
