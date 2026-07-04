//! Generic snapshot/restore + one-transaction journaled write for LOCAL v1 tools
//! (tasks/notes/reminders — Safe Operator Harness phase 1, local-journal-mechanism).
//!
//! Unlike a connector write (which cannot be transactional — it involves a network call),
//! a local mutation is entirely within this process's SQLite connection, so the outbox
//! insert + forward mutate + post_state_version write-back run in ONE `sqlx::Transaction`:
//! either all of it lands or none of it does, closing both the crash window AND the
//! kill-switch-mid-execute window that the connector path needs a separate M5 re-check for.
//!
//! `restore_row`/`clear_deleted_at` use a compile-time WHITELISTED column set per table —
//! never build SQL from arbitrary `pre_state` JSON keys, which would let a poisoned journal
//! row (ids come from LLM-parsed free text) inject arbitrary column writes.
//!
//! C10 concurrency guard: every mutating statement here carries `WHERE id = ? AND
//! updated_at = ?` and reports `rows_affected()` to the caller — a record changed under us
//! is detected by the UPDATE itself finding zero rows, never by a separate SELECT-then-UPDATE
//! (which would be a TOCTOU race).
use crate::queries::journal::{self, ActionJournalRow, NewAction};
use crate::DbHandle;
use anyhow::Result;
use serde_json::{Map, Value};
use sqlx::{Row, Sqlite, Transaction};

/// The three local tool tables this mechanism covers. A closed, compile-time set —
/// deliberately excludes memory (HNSW-index coupling) and calendar (recurrence undo
/// unresolved); see the phase's Risk Notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalTable {
    Tasks,
    Notes,
    Reminders,
}

impl LocalTable {
    pub fn table_name(self) -> &'static str {
        match self {
            LocalTable::Tasks => "tasks",
            LocalTable::Notes => "notes",
            LocalTable::Reminders => "reminders",
        }
    }

    /// Whitelisted columns eligible for `snapshot_row`/`restore_row`. Deliberately excludes
    /// `notes.embedding` (regenerable BLOB, never part of a pre-image) and `created_at`
    /// (immutable, never restored).
    fn whitelisted_columns(self) -> &'static [&'static str] {
        match self {
            LocalTable::Tasks => &[
                "id",
                "title",
                "description",
                "priority",
                "status",
                "due_at",
                "completed_at",
                "calendar_event_id",
                "domain_id",
                "updated_at",
                "deleted_at",
            ],
            LocalTable::Notes => &[
                "id", "title", "content", "tags", "wikilinks", "domain_id", "updated_at",
                "deleted_at",
            ],
            LocalTable::Reminders => &[
                "id",
                "title",
                "fire_at",
                "recurrence",
                "fired_at",
                "outcome",
                "outcome_at",
                "session_id",
                "updated_at",
                "deleted_at",
            ],
        }
    }
}

/// One concrete local mutation, matched to a fixed SQL statement inside this module — the
/// tool layer selects a variant and supplies its fields; no SQL is built outside
/// `haily-db/queries` (repo convention). Each variant is exactly the write an existing v1
/// tool already performs, now run against the shared transaction.
pub enum LocalMutation<'a> {
    TaskCreate {
        id: &'a str,
        title: &'a str,
        description: Option<&'a str>,
        priority: &'a str,
        due_at: Option<&'a str>,
    },
    TaskComplete {
        id: &'a str,
    },
    TaskDelete {
        id: &'a str,
    },
    NoteSave {
        id: &'a str,
        title: &'a str,
        content: &'a str,
        tags: Option<&'a str>,
        wikilinks: Option<&'a str>,
    },
    NoteUpdate {
        id: &'a str,
        title: &'a str,
        content: &'a str,
    },
    NoteDelete {
        id: &'a str,
    },
    ReminderAdd {
        id: &'a str,
        title: &'a str,
        fire_at: &'a str,
        recurrence: Option<&'a str>,
        session_id: &'a str,
    },
    ReminderDelete {
        id: &'a str,
    },
}

impl<'a> LocalMutation<'a> {
    fn table(&self) -> LocalTable {
        match self {
            LocalMutation::TaskCreate { .. }
            | LocalMutation::TaskComplete { .. }
            | LocalMutation::TaskDelete { .. } => LocalTable::Tasks,
            LocalMutation::NoteSave { .. }
            | LocalMutation::NoteUpdate { .. }
            | LocalMutation::NoteDelete { .. } => LocalTable::Notes,
            LocalMutation::ReminderAdd { .. } | LocalMutation::ReminderDelete { .. } => {
                LocalTable::Reminders
            }
        }
    }

    /// The row id this mutation targets. `None` only for a create with a not-yet-inserted
    /// row — but callers always mint the id up front (see `local_journaled_write`), so a
    /// create's id is known before this runs.
    fn row_id(&self) -> &'a str {
        match self {
            LocalMutation::TaskCreate { id, .. }
            | LocalMutation::TaskComplete { id }
            | LocalMutation::TaskDelete { id }
            | LocalMutation::NoteSave { id, .. }
            | LocalMutation::NoteUpdate { id, .. }
            | LocalMutation::NoteDelete { id }
            | LocalMutation::ReminderAdd { id, .. }
            | LocalMutation::ReminderDelete { id } => id,
        }
    }

    /// True for a create (no pre_state to snapshot — the row does not exist yet).
    fn is_create(&self) -> bool {
        matches!(
            self,
            LocalMutation::TaskCreate { .. }
                | LocalMutation::NoteSave { .. }
                | LocalMutation::ReminderAdd { .. }
        )
    }
}

/// Snapshot the whitelisted columns of `table`/`id` as a JSON object, run against the same
/// transaction as the caller's read (so a concurrent write cannot interleave). `None` if the
/// row does not exist.
pub async fn snapshot_row(
    tx: &mut Transaction<'_, Sqlite>,
    table: LocalTable,
    id: &str,
) -> Result<Option<Value>> {
    let cols = table.whitelisted_columns();
    let sql = format!(
        "SELECT {} FROM {} WHERE id = ?",
        cols.join(", "),
        table.table_name()
    );
    let row = sqlx::query(&sql)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.map(|r| row_to_json(&r, cols)))
}

/// True when the reminder `id` has already fired (`fired_at IS NOT NULL`). A fired reminder's
/// real-world side effect (the notification) already happened and cannot be un-sent, so the
/// undo layer treats it as `final` regardless of what the journal recorded at write time —
/// this is a LIVE check, not a journaled compensability, because a reminder can fire at any
/// point between its `reminder_add`/`reminder_delete` write and a later undo request.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn reminder_is_fired(db: &DbHandle, id: &str) -> Result<bool> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT fired_at FROM reminders WHERE id = ?")
            .bind(id)
            .fetch_optional(db.pool())
            .await?;
    Ok(row.and_then(|(f,)| f).is_some())
}

/// Pool-scoped existence check for a local row (id present, regardless of `deleted_at`) —
/// used by the reconciliation sweep's live-SELECT classification for a local orphan (C1),
/// which needs no transaction (it is a single read, not a read-then-write).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn row_exists(db: &DbHandle, table: LocalTable, id: &str) -> Result<bool> {
    let sql = format!("SELECT 1 FROM {} WHERE id = ?", table.table_name());
    Ok(sqlx::query(&sql)
        .bind(id)
        .fetch_optional(db.pool())
        .await?
        .is_some())
}

/// Read the columns named in `cols` off a raw sqlx row into a JSON object. Every whitelisted
/// column here is TEXT (RFC3339 strings, ids, or free text) — no BLOB is ever in the
/// whitelist, so `try_get::<Option<String>, _>` covers every column safely.
fn row_to_json(row: &sqlx::sqlite::SqliteRow, cols: &[&str]) -> Value {
    let mut map = Map::new();
    for col in cols {
        let val: Option<String> = row.try_get(*col).unwrap_or(None);
        map.insert(
            (*col).to_string(),
            val.map(Value::String).unwrap_or(Value::Null),
        );
    }
    Value::Object(map)
}

/// Restore a row's mutable fields from `pre_state` (as captured by `snapshot_row`), guarded
/// by `WHERE id = ? AND updated_at = ?` — C10: if the record's `updated_at` no longer matches
/// `expected_updated_at`, ZERO rows are affected and the caller must treat that as a refusal
/// (record changed under us), never retry with a fresh SELECT.
///
/// Only whitelisted, mutable columns present in `pre_state` are written; `id`/`updated_at`
/// are excluded from the SET list (id never changes; updated_at is always stamped to now by
/// this call, not restored from the pre-image, so a chain of undos keeps moving forward).
pub async fn restore_row(
    tx: &mut Transaction<'_, Sqlite>,
    table: LocalTable,
    id: &str,
    pre_state: &Value,
    expected_updated_at: &str,
) -> Result<u64> {
    let obj = pre_state.as_object();
    let settable: Vec<&str> = table
        .whitelisted_columns()
        .iter()
        .copied()
        .filter(|c| *c != "id" && *c != "updated_at")
        .filter(|c| obj.is_some_and(|o| o.contains_key(*c)))
        .collect();
    if settable.is_empty() {
        // Nothing to restore (a create's pre_state is None and never reaches here; an
        // update/delete pre_state always carries at least one mutable field).
        return Ok(0);
    }
    let now = chrono::Utc::now().to_rfc3339();
    let assignments: Vec<String> = settable.iter().map(|c| format!("{c} = ?")).collect();
    let sql = format!(
        "UPDATE {} SET {}, updated_at = ? WHERE id = ? AND updated_at = ?",
        table.table_name(),
        assignments.join(", ")
    );
    let mut q = sqlx::query(&sql);
    for col in &settable {
        let v = obj.and_then(|o| o.get(*col)).and_then(Value::as_str);
        q = q.bind(v);
    }
    q = q.bind(&now).bind(id).bind(expected_updated_at);
    Ok(q.execute(&mut **tx).await?.rows_affected())
}

/// Clear `deleted_at` (undo of a soft-delete / create-undo), guarded the same C10 way as
/// `restore_row`. Used both for undoing a delete (restore visibility) and undoing a create
/// (soft-delete the row it created) via `soft_delete_row`.
pub async fn clear_deleted_at(
    tx: &mut Transaction<'_, Sqlite>,
    table: LocalTable,
    id: &str,
    expected_updated_at: &str,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let sql = format!(
        "UPDATE {} SET deleted_at = NULL, updated_at = ? WHERE id = ? AND updated_at = ?",
        table.table_name()
    );
    Ok(sqlx::query(&sql)
        .bind(&now)
        .bind(id)
        .bind(expected_updated_at)
        .execute(&mut **tx)
        .await?
        .rows_affected())
}

/// Soft-delete a row (undo of a create), guarded the same C10 way as `restore_row`.
pub async fn soft_delete_row(
    tx: &mut Transaction<'_, Sqlite>,
    table: LocalTable,
    id: &str,
    expected_updated_at: &str,
) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let sql = format!(
        "UPDATE {} SET deleted_at = ?, updated_at = ? WHERE id = ? AND updated_at = ?",
        table.table_name()
    );
    Ok(sqlx::query(&sql)
        .bind(&now)
        .bind(&now)
        .bind(id)
        .bind(expected_updated_at)
        .execute(&mut **tx)
        .await?
        .rows_affected())
}

/// Read a row's live `updated_at`, run against the transaction so it observes the mutation
/// just performed on the SAME connection.
async fn read_updated_at(
    tx: &mut Transaction<'_, Sqlite>,
    table: LocalTable,
    id: &str,
) -> Result<Option<String>> {
    let sql = format!("SELECT updated_at FROM {} WHERE id = ?", table.table_name());
    let row = sqlx::query(&sql)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.and_then(|r| r.try_get::<Option<String>, _>("updated_at").ok().flatten()))
}

/// Apply the concrete forward mutation inside `tx`. Each arm mirrors the SQL an existing v1
/// tool already ran directly against the pool — now scoped to the shared transaction so it
/// commits atomically with the journal outbox row. Returns the PRIMARY statement's
/// `rows_affected()` so the caller can detect a no-op update/delete (target id not found)
/// and roll back rather than leave a phantom, un-undoable journal row for a write that never
/// happened.
async fn apply_mutation(tx: &mut Transaction<'_, Sqlite>, m: &LocalMutation<'_>) -> Result<u64> {
    let now = chrono::Utc::now().to_rfc3339();
    let affected = match m {
        LocalMutation::TaskCreate {
            id,
            title,
            description,
            priority,
            due_at,
        } => {
            sqlx::query(
                "INSERT INTO tasks (id, title, description, priority, status, due_at, created_at, updated_at)
                 VALUES (?, ?, ?, ?, 'todo', ?, ?, ?)",
            )
            .bind(*id)
            .bind(*title)
            .bind(*description)
            .bind(*priority)
            .bind(*due_at)
            .bind(&now)
            .bind(&now)
            .execute(&mut **tx)
            .await?
            .rows_affected()
        }
        LocalMutation::TaskComplete { id } => {
            sqlx::query(
                "UPDATE tasks SET status = 'done', completed_at = ?, updated_at = ? \
                 WHERE id = ? AND deleted_at IS NULL",
            )
            .bind(&now)
            .bind(&now)
            .bind(*id)
            .execute(&mut **tx)
            .await?
            .rows_affected()
        }
        LocalMutation::TaskDelete { id } => {
            sqlx::query(
                "UPDATE tasks SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
            )
            .bind(&now)
            .bind(&now)
            .bind(*id)
            .execute(&mut **tx)
            .await?
            .rows_affected()
        }
        LocalMutation::NoteSave {
            id,
            title,
            content,
            tags,
            wikilinks,
        } => {
            let affected = sqlx::query(
                "INSERT INTO notes (id, title, content, tags, wikilinks, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(*id)
            .bind(*title)
            .bind(*content)
            .bind(*tags)
            .bind(*wikilinks)
            .bind(&now)
            .bind(&now)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            // M3b: a second write (set_wikilinks) bumps updated_at again when wikilinks are
            // present — the SAME transaction, so `post_state_version` capture below still
            // reads the LAST write. When `wikilinks` is None the insert above already set
            // updated_at once and no second write is needed.
            if wikilinks.is_some() {
                sqlx::query("UPDATE notes SET wikilinks = ?, updated_at = ? WHERE id = ?")
                    .bind(*wikilinks)
                    .bind(&now)
                    .bind(*id)
                    .execute(&mut **tx)
                    .await?;
            }
            affected
        }
        LocalMutation::NoteUpdate {
            id,
            title,
            content,
        } => {
            sqlx::query(
                "UPDATE notes SET title = ?, content = ?, updated_at = ? \
                 WHERE id = ? AND deleted_at IS NULL",
            )
            .bind(*title)
            .bind(*content)
            .bind(&now)
            .bind(*id)
            .execute(&mut **tx)
            .await?
            .rows_affected()
        }
        LocalMutation::NoteDelete { id } => {
            sqlx::query(
                "UPDATE notes SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
            )
            .bind(&now)
            .bind(&now)
            .bind(*id)
            .execute(&mut **tx)
            .await?
            .rows_affected()
        }
        LocalMutation::ReminderAdd {
            id,
            title,
            fire_at,
            recurrence,
            session_id,
        } => {
            sqlx::query(
                "INSERT INTO reminders (id, title, fire_at, recurrence, session_id, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(*id)
            .bind(*title)
            .bind(*fire_at)
            .bind(*recurrence)
            .bind(*session_id)
            .bind(&now)
            .bind(&now)
            .execute(&mut **tx)
            .await?
            .rows_affected()
        }
        LocalMutation::ReminderDelete { id } => {
            sqlx::query(
                "UPDATE reminders SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
            )
            .bind(&now)
            .bind(&now)
            .bind(*id)
            .execute(&mut **tx)
            .await?
            .rows_affected()
        }
    };
    Ok(affected)
}

/// C2: journal-insert + forward-mutate + post_state_version write-back in ONE transaction.
///
/// Sequence: BEGIN → snapshot pre_state (skipped for a create — no row yet) → for an
/// update/delete, a MISSING target (pre_state absent) rolls back with NO journal row and
/// returns `Ok(None)` (the tool layer reports "not found", exactly like the pre-journal
/// behavior) → journal outbox insert (`pre_state` already redact/tag-stripped by the caller)
/// → apply the forward mutation → re-read the row's NEW `updated_at` (captured AFTER the
/// LAST write, satisfying M3b for notes-with-wikilinks) → write it back as
/// `post_state_version` → COMMIT.
///
/// A local write has no external version token, so `post_state_version` doubles as the C10
/// baseline for a LOCAL row (see `local_compensator::local_attempt_undo`), unlike the
/// connector path where it mirrors a third-party `write_date`.
///
/// # Errors
/// Any failure rolls back the whole transaction (sqlx drops an uncommitted `Transaction`),
/// so a failed mutate never leaves an orphaned journal row.
///
/// `turn_id` (Harness Completion phase 2) pushed this signature past clippy's default
/// 7-arg ceiling; every parameter maps 1:1 onto a `NewAction`/journal-insert field this
/// function's caller already owns individually (mirrors the same `#[allow]` precedent at
/// `OdooExecutorConfig::production`), so grouping into a param struct here would just move
/// the same fields into a second short-lived type with no clearer call sites.
#[allow(clippy::too_many_arguments)]
pub async fn local_journaled_write(
    db: &DbHandle,
    mutation: LocalMutation<'_>,
    session_id: &str,
    tool_name: &str,
    tool_tier: &str,
    request_params: &str,
    // Server-derived turn correlation id (migration 0016) — stamped on the outbox row so
    // `list_by_turn`/`undo_turn` can group this write with the rest of its turn. `None`
    // is valid (row excluded from any turn's group).
    turn_id: Option<&str>,
    retention_days: i64,
) -> Result<Option<(ActionJournalRow, String)>> {
    let table = mutation.table();
    let id = mutation.row_id().to_string();
    let mut tx = db.pool().begin().await?;

    let pre_state = if mutation.is_create() {
        None
    } else {
        match snapshot_row(&mut tx, table, &id).await? {
            Some(v) => Some(v),
            None => {
                // Target does not exist — nothing to journal, nothing to mutate. Roll back
                // (no-op on an otherwise-untouched transaction) and let the caller report a
                // "not found", matching the pre-journal tool behavior exactly.
                tx.rollback().await.ok();
                return Ok(None);
            }
        }
    };
    let pre_state_str = pre_state.as_ref().map(Value::to_string);

    let row = journal::insert_tx(
        &mut tx,
        NewAction {
            session_id,
            tool_name,
            tool_tier,
            // Local rows are legitimately reversible (soft-delete/update-in-place) — the
            // undo refusal set for them lives in `local_compensator`, not the connector
            // "final"/"compensatable" vocabulary, but the column is still populated for
            // consistency with the schema's NOT NULL constraint.
            compensability: "compensatable",
            idempotency_key: &format!("{tool_name}:{id}"),
            correlation_ref: &id,
            request_params,
            pre_state: pre_state_str.as_deref(),
            pre_state_version: None,
            compensation_plan: None,
            turn_id,
            retention_days,
        },
    )
    .await?;

    // Defense-in-depth beyond the pre-check above: `snapshot_row` does not filter
    // `deleted_at` (a delete/complete on an ALREADY-deleted row is exactly the case where the
    // row exists but the mutate's own `WHERE deleted_at IS NULL` affects zero rows). Roll back
    // rather than leave a journal row for a write that never actually happened.
    if apply_mutation(&mut tx, &mutation).await? == 0 {
        tx.rollback().await.ok();
        return Ok(None);
    }

    let post_updated_at = read_updated_at(&mut tx, table, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("local_journaled_write: row '{id}' vanished after its own mutation"))?;
    journal::set_post_state_version_tx(&mut tx, &row.id, &post_updated_at).await?;
    // A local write that reaches COMMIT is, by construction, verified — it is the SAME
    // connection that just performed it, not a third-party system reached over the network.
    // `match` here means "never left pending", so reconcile's live-SELECT classification
    // (see `local_compensator`) only ever has to handle a genuinely mid-transaction crash
    // (which leaves NO row at all, since sqlx rolls back an uncommitted transaction).
    journal::set_readback_tx(&mut tx, &row.id, "match", None).await?;

    tx.commit().await?;

    // Return the row as recorded pre-commit; post_state_version is set separately above
    // (mirrors the connector path's two-step insert-then-set_post_state_version shape).
    Ok(Some((row, post_updated_at)))
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
    async fn task_create_then_snapshot_and_restore_roundtrip() {
        let (db, _d) = db().await;
        let (row, _v) = local_journaled_write(
            &db,
            LocalMutation::TaskCreate {
                id: "task-1",
                title: "Buy milk",
                description: None,
                priority: "medium",
                due_at: None,
            },
            "sess-1",
            "task_create",
            "ReversibleWrite",
            "{}",
            None,
            30,
        )
        .await
        .unwrap()
        .expect("create always succeeds");
        assert!(row.pre_state.is_none(), "create has no pre_state");

        let mut tx = db.pool().begin().await.unwrap();
        let snap = snapshot_row(&mut tx, LocalTable::Tasks, "task-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snap["title"], "Buy milk");
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn restore_row_refuses_when_updated_at_stale() {
        let (db, _d) = db().await;
        let (_row, v0) = local_journaled_write(
            &db,
            LocalMutation::TaskCreate {
                id: "task-2",
                title: "Original",
                description: None,
                priority: "low",
                due_at: None,
            },
            "sess-1",
            "task_create",
            "ReversibleWrite",
            "{}",
            None,
            30,
        )
        .await
        .unwrap()
        .expect("create always succeeds");

        // A concurrent external edit bumps updated_at without going through this module.
        sqlx::query("UPDATE tasks SET title = 'Edited elsewhere', updated_at = ? WHERE id = ?")
            .bind(chrono::Utc::now().to_rfc3339())
            .bind("task-2")
            .execute(db.pool())
            .await
            .unwrap();

        let mut tx = db.pool().begin().await.unwrap();
        let pre = serde_json::json!({"title": "Original"});
        let affected = restore_row(&mut tx, LocalTable::Tasks, "task-2", &pre, &v0)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(affected, 0, "stale updated_at must refuse via rows_affected==0");
    }
}
