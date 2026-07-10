//! Action journal queries — the persistence layer for the undo/reconcile state machine.
//!
//! All writes go through here so the append-only trigger (migration 0012) is the single
//! enforcement point. Evidentiary columns (request_params/pre_state/pre_state_version/
//! created_at/idempotency_key) are write-once at `insert`; processing columns advance via
//! `set_readback`/`advance_undo_status`/`increment_undo_attempt`.
use crate::DbHandle;
use anyhow::Result;
use serde::Serialize;
use sqlx::{Executor, FromRow, Sqlite, Transaction};
use uuid::Uuid;

/// A single recorded connector write. `pre_state`/`post_state`/`compensation_plan` are
/// opaque JSON strings; the tool layer owns their shape. `request_params` is already
/// REDACTED (C4) and third-party strings are tag-stripped (C5) by the caller BEFORE it
/// reaches `insert` — this layer never sees a raw secret or a live `<tool_call>` tag.
///
/// `Serialize` (phase 6) lets this cross the Tauri IPC boundary unchanged for the GUI's
/// recent-actions/undo surface (`list_journal`) — read-only exposure, no new write path.
#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionJournalRow {
    pub id: String,
    pub session_id: String,
    pub tool_name: String,
    pub tool_tier: String,
    pub compensability: String,
    pub idempotency_key: String,
    pub correlation_ref: String,
    pub request_params: String,
    pub pre_state: Option<String>,
    pub pre_state_version: Option<String>,
    pub post_state: Option<String>,
    /// Server-derived correlation id shared by every journal row written during ONE agent
    /// turn (migration 0016) — lets `list_by_turn` collect a turn's writes for group undo.
    /// `None` for any row written before this column existed, or by a caller that never
    /// threaded a turn identity through.
    pub turn_id: Option<String>,
    /// The opaque version token (Odoo `write_date`) AS OF our forward write's completion —
    /// the C10 concurrency baseline for a self-undo. Set post-write (mutable, migration 0014),
    /// so the undo refuses only on a THIRD-PARTY change beyond our own write. `None` until the
    /// post-write read-back lands (creates, lost responses) → the guard falls back to
    /// `pre_state_version` or skips.
    pub post_state_version: Option<String>,
    pub readback_status: String,
    pub compensation_plan: Option<String>,
    pub undo_status: String,
    pub undo_attempts: i64,
    pub created_at: String,
    pub undone_at: Option<String>,
    pub retention_expires_at: String,
    /// M2 (Activate-and-Measure phase 4b): the owning connector manifest's content hash,
    /// pinned at outbox-insert time. `None` for a local row (no manifest) or a row written
    /// before migration 0019. Compared against the manifest's CURRENT hash at undo/reconcile
    /// time — a mismatch means the manifest moved/changed since this write and the
    /// compensation must refuse rather than target a base_url/schema the write never touched.
    pub manifest_hash: Option<String>,
    /// The `coding_workspaces.id` a coding-tool write executed inside (migration 0025).
    /// `None` for every non-coding row (local personal tools, connector writes) and any
    /// row written before that migration. Audit/grouping only — the worktree, not this
    /// column, is the authoritative compensator for a coding change.
    pub workspace_id: Option<String>,
}

/// Fields required to record a write. Grouped so `insert` stays within a sane arity and
/// the outbox call-site reads as one struct literal at the point BEFORE the external call.
pub struct NewAction<'a> {
    pub session_id: &'a str,
    pub tool_name: &'a str,
    pub tool_tier: &'a str,
    pub compensability: &'a str,
    pub idempotency_key: &'a str,
    pub correlation_ref: &'a str,
    /// Already redacted (C4).
    pub request_params: &'a str,
    /// Already tag-stripped (C5).
    pub pre_state: Option<&'a str>,
    pub pre_state_version: Option<&'a str>,
    pub compensation_plan: Option<&'a str>,
    /// Server-derived turn correlation id (migration 0016) — see `ActionJournalRow::turn_id`.
    /// `None` is valid (row excluded from any turn's undo group, never mis-grouped).
    pub turn_id: Option<&'a str>,
    /// Days until PII in this row is eligible for purge.
    pub retention_days: i64,
    /// M2 (Activate-and-Measure phase 4b) — see `ActionJournalRow::manifest_hash`. `None`
    /// for a local row (no connector manifest exists to pin).
    pub manifest_hash: Option<&'a str>,
}

/// Shared insert body for [`insert`]/[`insert_tx`] — generic over any sqlx `Executor` so the
/// pool and transaction-scoped callers share one copy of the SQL/bind-list instead of two
/// hand-kept-in-sync copies. `workspace_id` is threaded as a separate parameter rather than a
/// `NewAction` field so the coding-tool audit path (the only writer that sets it) can record
/// it without forcing `workspace_id: None` onto every existing connector/local `NewAction`
/// literal in the workspace.
async fn insert_via<'e, E>(
    exec: E,
    a: NewAction<'_>,
    workspace_id: Option<&str>,
) -> Result<ActionJournalRow>
where
    E: Executor<'e, Database = Sqlite>,
{
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let created_at = now.to_rfc3339();
    let retention_expires_at = (now + chrono::Duration::days(a.retention_days)).to_rfc3339();
    Ok(sqlx::query_as::<_, ActionJournalRow>(
        "INSERT INTO action_journal
             (id, session_id, tool_name, tool_tier, compensability, idempotency_key,
              correlation_ref, request_params, pre_state, pre_state_version,
              readback_status, compensation_plan, undo_status, undo_attempts,
              created_at, retention_expires_at, turn_id, manifest_hash, workspace_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, 'not_requested', 0, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(a.session_id)
    .bind(a.tool_name)
    .bind(a.tool_tier)
    .bind(a.compensability)
    .bind(a.idempotency_key)
    .bind(a.correlation_ref)
    .bind(a.request_params)
    .bind(a.pre_state)
    .bind(a.pre_state_version)
    .bind(a.compensation_plan)
    .bind(&created_at)
    .bind(&retention_expires_at)
    .bind(a.turn_id)
    .bind(a.manifest_hash)
    .bind(workspace_id)
    .fetch_one(exec)
    .await?)
}

/// Outbox insert — MUST be called BEFORE the external write so a crash mid-write still
/// leaves the compensation_plan + pre_state on disk for reconciliation.
///
/// # Errors
/// Returns an error on a UNIQUE conflict on `idempotency_key` (a duplicate submit of the
/// same logical op) or any DB failure.
pub async fn insert(db: &DbHandle, a: NewAction<'_>) -> Result<ActionJournalRow> {
    insert_via(db.pool(), a, None).await
}

/// Coding-tool audit insert (Sub-Agent + Skill Architecture phase 1): identical to [`insert`]
/// but stamps `workspace_id` so the row can be grouped by its owning `CodingWorkspace`. This
/// is an AUDIT row — the worktree, not this row, is the authoritative compensator for the
/// file change (a coding undo is a `git checkout -- . && git clean -ffdx`, never a DB
/// restore), so no `compensation_plan`/`pre_state` snapshot of file bytes is stored.
///
/// # Errors
/// Returns an error on a UNIQUE conflict on `idempotency_key` or any DB failure.
pub async fn insert_coding_audit(
    db: &DbHandle,
    a: NewAction<'_>,
    workspace_id: &str,
) -> Result<ActionJournalRow> {
    insert_via(db.pool(), a, Some(workspace_id)).await
}

/// All coding-tool audit rows for one workspace, newest first (JournalBrowser grouping).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_by_workspace(
    db: &DbHandle,
    workspace_id: &str,
) -> Result<Vec<ActionJournalRow>> {
    Ok(sqlx::query_as::<_, ActionJournalRow>(
        "SELECT * FROM action_journal WHERE workspace_id = ? ORDER BY created_at DESC",
    )
    .bind(workspace_id)
    .fetch_all(db.pool())
    .await?)
}

/// Transaction-scoped variant of [`insert`] — the LOCAL-tool write path (phase 1) needs the
/// outbox insert to commit atomically with its own forward mutate (C2), unlike a connector
/// write which cannot be transactional (it involves a network call between insert and
/// read-back). Identical semantics and columns to `insert`, just bound to a caller-owned
/// `Transaction` instead of the pool.
///
/// # Errors
/// Returns an error on a UNIQUE conflict on `idempotency_key` or any DB failure.
pub async fn insert_tx(
    tx: &mut Transaction<'_, Sqlite>,
    a: NewAction<'_>,
) -> Result<ActionJournalRow> {
    insert_via(&mut **tx, a, None).await
}

/// Shared update body for [`set_readback`]/[`set_readback_tx`] — one copy of the SQL for
/// both the pool-scoped and transaction-scoped callers.
async fn set_readback_via<'e, E>(
    exec: E,
    id: &str,
    readback_status: &str,
    post_state: Option<&str>,
) -> Result<()>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query("UPDATE action_journal SET readback_status = ?, post_state = ? WHERE id = ?")
        .bind(readback_status)
        .bind(post_state)
        .bind(id)
        .execute(exec)
        .await?;
    Ok(())
}

/// Record the read-back verdict + post_state after the external write (or during a
/// reconciliation sweep). `post_state` is tag-stripped by the caller.
///
/// # Errors
/// Returns an error if the update fails. Silently succeeds if no row matches `id`.
pub async fn set_readback(
    db: &DbHandle,
    id: &str,
    readback_status: &str,
    post_state: Option<&str>,
) -> Result<()> {
    set_readback_via(db.pool(), id, readback_status, post_state).await
}

/// Rewrite the `compensation_plan` after the external call, once the created record's id
/// is known (a create's plan is journaled BEFORE the call with no id — the id it RETURNS
/// must be written back or the archive/write compensation has no target). `compensation_plan`
/// is a PROCESSING column, deliberately outside the migration-0012 append-only trigger
/// (which guards only request_params/pre_state/pre_state_version/created_at/idempotency_key),
/// so this update is permitted; the evidentiary columns remain immutable.
///
/// # Errors
/// Returns an error if the update fails. Silently succeeds if no row matches `id`.
pub async fn update_compensation_plan(db: &DbHandle, id: &str, plan_json: &str) -> Result<()> {
    sqlx::query("UPDATE action_journal SET compensation_plan = ? WHERE id = ?")
        .bind(plan_json)
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Record the post-write version token (Odoo `write_date`) captured by the post-write
/// read-back — the C10 self-undo concurrency baseline. `post_state_version` is a PROCESSING
/// column (migration 0014, outside the 0012 append-only trigger), so this update is permitted.
///
/// # Errors
/// Returns an error if the update fails. Silently succeeds if no row matches `id`.
pub async fn set_post_state_version(db: &DbHandle, id: &str, version: &str) -> Result<()> {
    set_post_state_version_via(db.pool(), id, version).await
}

/// Shared update body for [`set_post_state_version`]/[`set_post_state_version_tx`].
async fn set_post_state_version_via<'e, E>(exec: E, id: &str, version: &str) -> Result<()>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query("UPDATE action_journal SET post_state_version = ? WHERE id = ?")
        .bind(version)
        .bind(id)
        .execute(exec)
        .await?;
    Ok(())
}

/// Transaction-scoped variant of [`set_readback`] — the local write path (C2) sets
/// `readback_status` to `match` INSIDE the same transaction as the forward mutate: a
/// committed local write is, by construction, verified (it is the same SQLite connection
/// that just performed it, not a third-party system reached over the network), so there is
/// no separate post-write GET/diff step to run outside the transaction.
///
/// # Errors
/// Returns an error if the update fails. Silently succeeds if no row matches `id`.
pub async fn set_readback_tx(
    tx: &mut Transaction<'_, Sqlite>,
    id: &str,
    readback_status: &str,
    post_state: Option<&str>,
) -> Result<()> {
    set_readback_via(&mut **tx, id, readback_status, post_state).await
}

/// Transaction-scoped variant of [`set_post_state_version`] — used by the local write path
/// (C2) so the version write-back commits atomically with the outbox insert + forward
/// mutate rather than as a separate post-commit statement.
///
/// # Errors
/// Returns an error if the update fails. Silently succeeds if no row matches `id`.
pub async fn set_post_state_version_tx(
    tx: &mut Transaction<'_, Sqlite>,
    id: &str,
    version: &str,
) -> Result<()> {
    set_post_state_version_via(&mut **tx, id, version).await
}

/// Advance the undo state machine. `undone_at` is set only on the terminal `undone`.
///
/// # Errors
/// Returns an error if the update fails. Silently succeeds if no row matches `id`.
pub async fn advance_undo_status(db: &DbHandle, id: &str, undo_status: &str) -> Result<()> {
    let undone_at = if undo_status == "undone" {
        Some(chrono::Utc::now().to_rfc3339())
    } else {
        None
    };
    sqlx::query(
        "UPDATE action_journal
         SET undo_status = ?, undone_at = COALESCE(?, undone_at)
         WHERE id = ?",
    )
    .bind(undo_status)
    .bind(undone_at)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Bump `undo_attempts` by one and return the new count (for the N=3 cap check).
///
/// # Errors
/// Returns an error if the update/read fails.
pub async fn increment_undo_attempt(db: &DbHandle, id: &str) -> Result<i64> {
    sqlx::query("UPDATE action_journal SET undo_attempts = undo_attempts + 1 WHERE id = ?")
        .bind(id)
        .execute(db.pool())
        .await?;
    let row = sqlx::query_as::<_, (i64,)>("SELECT undo_attempts FROM action_journal WHERE id = ?")
        .bind(id)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.map(|(n,)| n).unwrap_or(0))
}

/// Fetch one row by id. `None` if it does not exist.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get_by_id(db: &DbHandle, id: &str) -> Result<Option<ActionJournalRow>> {
    Ok(
        sqlx::query_as::<_, ActionJournalRow>("SELECT * FROM action_journal WHERE id = ?")
            .bind(id)
            .fetch_optional(db.pool())
            .await?,
    )
}

/// Session-scoped variant of [`get_by_id`] (M1) — `None` both when the id does not exist AND
/// when it belongs to a DIFFERENT session, so a caller cannot distinguish "not found" from
/// "not yours" by timing/response shape. Journal ids are parsed by the LLM out of free text
/// (a note/task the user wrote), so this is a security boundary, not a nicety: without it, a
/// crafted id from another session's journal could be undone by session A.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get_by_id_scoped(
    db: &DbHandle,
    id: &str,
    session_id: &str,
) -> Result<Option<ActionJournalRow>> {
    Ok(sqlx::query_as::<_, ActionJournalRow>(
        "SELECT * FROM action_journal WHERE id = ? AND session_id = ?",
    )
    .bind(id)
    .bind(session_id)
    .fetch_optional(db.pool())
    .await?)
}

/// Fetch one row by its idempotency key — used to detect a retry of a known op.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get_by_idempotency_key(db: &DbHandle, key: &str) -> Result<Option<ActionJournalRow>> {
    Ok(sqlx::query_as::<_, ActionJournalRow>(
        "SELECT * FROM action_journal WHERE idempotency_key = ?",
    )
    .bind(key)
    .fetch_optional(db.pool())
    .await?)
}

/// All rows for a session, newest first.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_by_session(db: &DbHandle, session_id: &str) -> Result<Vec<ActionJournalRow>> {
    Ok(sqlx::query_as::<_, ActionJournalRow>(
        "SELECT * FROM action_journal WHERE session_id = ? ORDER BY created_at DESC",
    )
    .bind(session_id)
    .fetch_all(db.pool())
    .await?)
}

/// All rows sharing `turn_id`, scoped to `session_id` (M1 — mirrors `get_by_id_scoped`):
/// a `turn_id` is only ever meaningful within the session that minted it, and scoping
/// here (rather than trusting the caller to pre-filter) means a cross-session `turn_id`
/// collision or a forged id from another session's turn yields an empty result, not a
/// leak of that session's rows. Used by `undo_turn` to collect the group before calling
/// the existing `batch_undo` (no ordering — see the local-write FK-free rationale there).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_by_turn(
    db: &DbHandle,
    turn_id: &str,
    session_id: &str,
) -> Result<Vec<ActionJournalRow>> {
    Ok(sqlx::query_as::<_, ActionJournalRow>(
        "SELECT * FROM action_journal WHERE turn_id = ? AND session_id = ? ORDER BY created_at ASC",
    )
    .bind(turn_id)
    .bind(session_id)
    .fetch_all(db.pool())
    .await?)
}

/// Rows still `pending` read-back past a grace window — orphans the startup
/// reconciliation sweep must classify (C6). The grace window avoids racing a write that
/// is legitimately still in flight at boot.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_incomplete(db: &DbHandle, grace_secs: i64) -> Result<Vec<ActionJournalRow>> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(grace_secs)).to_rfc3339();
    Ok(sqlx::query_as::<_, ActionJournalRow>(
        "SELECT * FROM action_journal
         WHERE readback_status = 'pending' AND created_at <= ?
         ORDER BY created_at ASC",
    )
    .bind(&cutoff)
    .fetch_all(db.pool())
    .await?)
}

/// Purge PII-bearing rows past their retention window. Returns the number removed.
///
/// Relies on the migration NOT installing a blanket DELETE trigger (0012 note).
///
/// # Errors
/// Returns an error if the delete fails.
pub async fn purge_expired(db: &DbHandle) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query("DELETE FROM action_journal WHERE retention_expires_at <= ?")
        .bind(&now)
        .execute(db.pool())
        .await?
        .rows_affected();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `src-tauri`'s `list_journal` command (phase 6) serializes rows straight over the
    /// Tauri IPC boundary, and the frontend's `JournalEntry` type (`src/lib/tauri.ts`)
    /// expects camelCase keys — proves the `#[serde(rename_all = "camelCase")]` derive
    /// actually produces that shape rather than the Rust snake_case field names.
    #[test]
    fn action_journal_row_serializes_to_camel_case() {
        let row = ActionJournalRow {
            id: "j1".into(),
            session_id: "s1".into(),
            tool_name: "odoo_create".into(),
            tool_tier: "IrreversibleWrite".into(),
            compensability: "compensatable".into(),
            idempotency_key: "idem-1".into(),
            correlation_ref: "corr-1".into(),
            request_params: "{}".into(),
            pre_state: None,
            pre_state_version: None,
            post_state: None,
            turn_id: None,
            post_state_version: None,
            readback_status: "pending".into(),
            compensation_plan: Some(r#"{"op":"unlink"}"#.into()),
            undo_status: "not_requested".into(),
            undo_attempts: 0,
            created_at: "2026-07-03T00:00:00Z".into(),
            undone_at: None,
            retention_expires_at: "2026-08-02T00:00:00Z".into(),
            manifest_hash: None,
            workspace_id: None,
        };
        let json = serde_json::to_value(&row).expect("serialize");
        assert_eq!(json["sessionId"], "s1");
        assert_eq!(json["toolName"], "odoo_create");
        assert_eq!(json["compensationPlan"], r#"{"op":"unlink"}"#);
        assert_eq!(json["undoStatus"], "not_requested");
        // Absent keys must serialize as JSON null, not be dropped, so the frontend's
        // `JournalEntry` (which types these as `string | null`) never sees `undefined`.
        assert_eq!(json["preState"], serde_json::Value::Null);
        assert!(
            json.get("tool_name").is_none(),
            "snake_case key must not also appear"
        );
    }
}
