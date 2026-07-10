//! Coding workspace lifecycle rows (Sub-Agent + Skill Architecture phase 1).
//!
//! A `CodingWorkspaceRow` is metadata for one ephemeral git worktree bound to a session
//! (and optionally a work_item). It records WHICH worktree existed for WHICH repo/branch —
//! it never stores file content or diffs. The worktree itself is the authoritative
//! compensator for in-workspace changes (a coding undo is `git checkout -- . && git clean
//! -ffdx`, not a DB restore), so this table is audit/lifecycle only.
//!
//! Mirrors `work_items` query idioms: soft-delete via `deleted_at`, all reads guarded by
//! `deleted_at IS NULL`, `rows_affected()`-based double-delete detection.
use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct CodingWorkspaceRow {
    pub id: String,
    pub session_id: String,
    /// Absolute path of the TARGET repo the worktree was cut from.
    pub repo_path: String,
    /// Branch the worktree checked out (workspace-local).
    pub branch: String,
    /// Absolute path of the ephemeral worktree.
    pub worktree_path: String,
    pub work_item_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

/// Persist a new workspace row. `id` is minted by the caller so the on-disk worktree and the
/// row share one id (the worktree dir is named after it).
///
/// # Errors
/// Returns an error if `session_id` does not reference a valid session, if `work_item_id` is
/// `Some` but does not reference a valid work item, or if the insert fails.
pub async fn create(
    db: &DbHandle,
    id: &str,
    session_id: &str,
    repo_path: &str,
    branch: &str,
    worktree_path: &str,
    work_item_id: Option<&str>,
) -> Result<CodingWorkspaceRow> {
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, CodingWorkspaceRow>(
        "INSERT INTO coding_workspaces
             (id, session_id, repo_path, branch, worktree_path, work_item_id,
              created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(id)
    .bind(session_id)
    .bind(repo_path)
    .bind(branch)
    .bind(worktree_path)
    .bind(work_item_id)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// Fetch one active (non-deleted) workspace by id. `None` if it does not exist or was
/// discarded.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get(db: &DbHandle, id: &str) -> Result<Option<CodingWorkspaceRow>> {
    Ok(sqlx::query_as::<_, CodingWorkspaceRow>(
        "SELECT * FROM coding_workspaces WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await?)
}

/// Session-scoped variant of [`get`] (mirrors `journal::get_by_id_scoped`) — `None` both when
/// the id does not exist AND when it belongs to a DIFFERENT session, so a workspace id parsed
/// out of LLM/tool text can never reach another session's worktree.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get_scoped(
    db: &DbHandle,
    id: &str,
    session_id: &str,
) -> Result<Option<CodingWorkspaceRow>> {
    Ok(sqlx::query_as::<_, CodingWorkspaceRow>(
        "SELECT * FROM coding_workspaces WHERE id = ? AND session_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(session_id)
    .fetch_optional(db.pool())
    .await?)
}

/// All active (non-deleted) workspaces, oldest first — the set the orphan-worktree GC (P4)
/// reconciles filesystem worktrees against.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_active(db: &DbHandle) -> Result<Vec<CodingWorkspaceRow>> {
    Ok(sqlx::query_as::<_, CodingWorkspaceRow>(
        "SELECT * FROM coding_workspaces WHERE deleted_at IS NULL ORDER BY created_at ASC",
    )
    .fetch_all(db.pool())
    .await?)
}

/// Soft-delete a workspace row (the on-disk worktree is torn down separately by the
/// compensator). Guarded by `deleted_at IS NULL` so a double-discard is detected via
/// `rows_affected()` rather than a separate SELECT.
///
/// Returns `true` if a row was actually deleted, `false` if `id` did not match an active row.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn soft_delete(db: &DbHandle, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE coding_workspaces SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// Mint a fresh workspace id (exposed so a caller can name the on-disk worktree dir before
/// persisting the row).
pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}
