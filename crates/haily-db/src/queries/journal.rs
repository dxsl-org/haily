//! Action journal queries — the persistence layer for the undo/reconcile state machine.
//!
//! All writes go through here so the append-only trigger (migration 0012) is the single
//! enforcement point. Evidentiary columns (request_params/pre_state/pre_state_version/
//! created_at/idempotency_key) are write-once at `insert`; processing columns advance via
//! `set_readback`/`advance_undo_status`/`increment_undo_attempt`.
use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

/// A single recorded connector write. `pre_state`/`post_state`/`compensation_plan` are
/// opaque JSON strings; the tool layer owns their shape. `request_params` is already
/// REDACTED (C4) and third-party strings are tag-stripped (C5) by the caller BEFORE it
/// reaches `insert` — this layer never sees a raw secret or a live `<tool_call>` tag.
#[derive(Debug, Clone, FromRow)]
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
    /// Days until PII in this row is eligible for purge.
    pub retention_days: i64,
}

/// Outbox insert — MUST be called BEFORE the external write so a crash mid-write still
/// leaves the compensation_plan + pre_state on disk for reconciliation.
///
/// # Errors
/// Returns an error on a UNIQUE conflict on `idempotency_key` (a duplicate submit of the
/// same logical op) or any DB failure.
pub async fn insert(db: &DbHandle, a: NewAction<'_>) -> Result<ActionJournalRow> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let created_at = now.to_rfc3339();
    let retention_expires_at = (now + chrono::Duration::days(a.retention_days)).to_rfc3339();
    Ok(sqlx::query_as::<_, ActionJournalRow>(
        "INSERT INTO action_journal
             (id, session_id, tool_name, tool_tier, compensability, idempotency_key,
              correlation_ref, request_params, pre_state, pre_state_version,
              readback_status, compensation_plan, undo_status, undo_attempts,
              created_at, retention_expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, 'not_requested', 0, ?, ?)
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
    .fetch_one(db.pool())
    .await?)
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
    sqlx::query("UPDATE action_journal SET readback_status = ?, post_state = ? WHERE id = ?")
        .bind(readback_status)
        .bind(post_state)
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
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
    sqlx::query("UPDATE action_journal SET post_state_version = ? WHERE id = ?")
        .bind(version)
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
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
