//! Review-findings history persistence (Sub-Agent + Skill Architecture phase 8).
//!
//! Records each persisted review [`Finding`](crate) so the pipeline's recurrence detector can
//! find a RECURRING class of problem (≥2 same-class findings across runs) at Ship and raise a
//! distillation proposal. Class key = `(category, module)`; recurrence is scoped by workspace
//! when a `workspace_id` is present, else by session. All SQL lives here (leaf-crate rule);
//! class-key derivation (`module_key`) is domain logic and lives in `haily_kms::distillation`.
use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

/// One persisted review finding row.
#[derive(Debug, Clone, FromRow)]
pub struct ReviewFinding {
    pub id: String,
    pub run_id: String,
    pub session_id: String,
    pub workspace_id: Option<String>,
    pub category: String,
    pub module: String,
    pub severity: String,
    pub file: String,
    pub summary: String,
    pub created_at: String,
}

/// The fields a caller supplies to record one finding — grouped so [`insert_finding`] stays
/// within a sane arity (the `NewAction`/`TraceMetrics` idiom).
pub struct NewReviewFinding<'a> {
    pub run_id: &'a str,
    pub session_id: &'a str,
    pub workspace_id: Option<&'a str>,
    pub category: &'a str,
    pub module: &'a str,
    pub severity: &'a str,
    pub file: &'a str,
    pub summary: &'a str,
}

/// One `(category, module)` recurrence bucket with its across-runs count.
#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct FindingClassCount {
    pub category: String,
    pub module: String,
    pub count: i64,
}

/// Insert one review finding row.
///
/// # Errors
/// Returns an error if `run_id`/`session_id` do not reference valid rows or the insert fails.
pub async fn insert_finding(db: &DbHandle, f: NewReviewFinding<'_>) -> Result<ReviewFinding> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, ReviewFinding>(
        "INSERT INTO review_findings
             (id, run_id, session_id, workspace_id, category, module, severity, file, summary, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(f.run_id)
    .bind(f.session_id)
    .bind(f.workspace_id)
    .bind(f.category)
    .bind(f.module)
    .bind(f.severity)
    .bind(f.file)
    .bind(f.summary)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// Recurring `(category, module)` classes for a workspace: classes with at least `min_count`
/// findings recorded (across all runs). The recurrence signal that drives a distillation
/// proposal at Ship (≥2 same-class across runs).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn recurrent_classes_for_workspace(
    db: &DbHandle,
    workspace_id: &str,
    min_count: i64,
) -> Result<Vec<FindingClassCount>> {
    Ok(sqlx::query_as::<_, FindingClassCount>(
        "SELECT category, module, COUNT(*) AS count
         FROM review_findings
         WHERE workspace_id = ?
         GROUP BY category, module
         HAVING COUNT(*) >= ?
         ORDER BY count DESC",
    )
    .bind(workspace_id)
    .bind(min_count)
    .fetch_all(db.pool())
    .await?)
}

/// Recurring `(category, module)` classes for a session (the fallback when no `workspace_id`
/// is available). Same semantics as [`recurrent_classes_for_workspace`].
///
/// # Errors
/// Returns an error if the query fails.
pub async fn recurrent_classes_for_session(
    db: &DbHandle,
    session_id: &str,
    min_count: i64,
) -> Result<Vec<FindingClassCount>> {
    Ok(sqlx::query_as::<_, FindingClassCount>(
        "SELECT category, module, COUNT(*) AS count
         FROM review_findings
         WHERE session_id = ?
         GROUP BY category, module
         HAVING COUNT(*) >= ?
         ORDER BY count DESC",
    )
    .bind(session_id)
    .bind(min_count)
    .fetch_all(db.pool())
    .await?)
}

/// The most recent findings for one `(category, module)` class in a workspace, newest first —
/// the raw material the distillation proposal renderer itemizes into rules. Capped at `limit`.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn findings_for_class(
    db: &DbHandle,
    workspace_id: &str,
    category: &str,
    module: &str,
    limit: i64,
) -> Result<Vec<ReviewFinding>> {
    Ok(sqlx::query_as::<_, ReviewFinding>(
        "SELECT * FROM review_findings
         WHERE workspace_id = ? AND category = ? AND module = ?
         ORDER BY created_at DESC LIMIT ?",
    )
    .bind(workspace_id)
    .bind(category)
    .bind(module)
    .bind(limit)
    .fetch_all(db.pool())
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::{pipeline_runs, sessions};

    async fn setup() -> (DbHandle, String, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        let session_id = Uuid::new_v4().to_string();
        sessions::create_session(&db, &session_id, "test", None)
            .await
            .unwrap();
        let run = pipeline_runs::create(&db, &session_id, None, 4)
            .await
            .unwrap();
        (db, session_id, run.id, dir)
    }

    fn finding<'a>(
        run: &'a str,
        sid: &'a str,
        ws: &'a str,
        cat: &'a str,
        module: &'a str,
        summary: &'a str,
    ) -> NewReviewFinding<'a> {
        NewReviewFinding {
            run_id: run,
            session_id: sid,
            workspace_id: Some(ws),
            category: cat,
            module,
            severity: cat,
            file: "crates/x/src/lib.rs",
            summary,
        }
    }

    #[tokio::test]
    async fn recurrence_requires_min_count_same_class() {
        let (db, sid, run, _dir) = setup().await;
        let ws = "ws-1";
        // Two findings of the SAME class → recurs; one of a different class → does not.
        insert_finding(
            &db,
            finding(&run, &sid, ws, "critical", "crates/core", "unwrap a"),
        )
        .await
        .unwrap();
        insert_finding(
            &db,
            finding(&run, &sid, ws, "critical", "crates/core", "unwrap b"),
        )
        .await
        .unwrap();
        insert_finding(
            &db,
            finding(&run, &sid, ws, "high", "crates/db", "n+1 once"),
        )
        .await
        .unwrap();

        let classes = recurrent_classes_for_workspace(&db, ws, 2).await.unwrap();
        assert_eq!(classes.len(), 1, "only the class with >=2 findings recurs");
        assert_eq!(classes[0].category, "critical");
        assert_eq!(classes[0].module, "crates/core");
        assert_eq!(classes[0].count, 2);

        // findings_for_class returns the raw material for the proposal.
        let rows = findings_for_class(&db, ws, "critical", "crates/core", 5)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
    }
}
