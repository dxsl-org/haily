//! C1 dispatch split: undo/reconcile for the THREE local v1 tool families (tasks, notes,
//! reminders) — a self-contained path that never touches a `ConnectorExecutor`.
//!
//! `is_local_row` MUST be checked BEFORE `refusal_reason` in `attempt_undo` and before the
//! executor read-back in `reconcile_incomplete`: the connector refusal set REFUSES any row
//! with `compensation_plan.is_none()`, which every local row legitimately has (local rows
//! carry no external compensation plan — they are restored from `pre_state` directly).
//!
//! `LOCAL_TOOL_TABLES` is a CLOSED compile-time allowlist covering only tasks/notes/reminders
//! — memory (HNSW-index coupling, C3) and calendar (recurrence undo unresolved) are
//! deliberately excluded and stay on their current tier/path.
use anyhow::Result;
use haily_db::queries::journal::{self, ActionJournalRow};
use haily_db::queries::local_snapshot::{self, LocalTable};
use haily_db::DbHandle;
use serde_json::Value;

use super::logic::UndoOutcome;

/// tool_name -> table mapping for every LOCAL tool this mechanism covers. A NULL-plan row
/// whose `tool_name` is NOT in this list (e.g. a connector create that crashed before its
/// plan write-back landed) falls through to the CONNECTOR path — never `local_attempt_undo`.
const LOCAL_TOOL_TABLES: &[(&str, LocalTable)] = &[
    ("task_create", LocalTable::Tasks),
    ("task_complete", LocalTable::Tasks),
    ("task_delete", LocalTable::Tasks),
    ("note_save", LocalTable::Notes),
    ("note_update", LocalTable::Notes),
    ("note_delete", LocalTable::Notes),
    ("reminder_add", LocalTable::Reminders),
    ("reminder_delete", LocalTable::Reminders),
];

/// The kind of forward mutation a local tool performed — decides HOW to invert it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalOpKind {
    /// The tool created the row. Undo = soft-delete it.
    Create,
    /// The tool changed fields on an existing row. Undo = restore the pre_state fields.
    Update,
    /// The tool soft-deleted the row. Undo = clear `deleted_at`.
    Delete,
}

fn op_kind(tool_name: &str) -> Option<LocalOpKind> {
    match tool_name {
        "task_create" | "note_save" | "reminder_add" => Some(LocalOpKind::Create),
        "task_complete" | "note_update" => Some(LocalOpKind::Update),
        "task_delete" | "note_delete" | "reminder_delete" => Some(LocalOpKind::Delete),
        _ => None,
    }
}

/// The `LocalTable` a local tool's `tool_name` maps to, or `None` if it is not in the closed
/// allowlist. Exposed (not just `is_local_row`) so `reconcile_incomplete` can locate the
/// live row to classify without duplicating the tool_name→table mapping.
pub fn local_table_for(tool_name: &str) -> Option<LocalTable> {
    LOCAL_TOOL_TABLES
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, t)| *t)
}

/// True when `row` was written by a LOCAL tool (tasks/notes/reminders) rather than a
/// connector. Checked BEFORE any connector-specific refusal/read-back logic in both
/// `attempt_undo` and `reconcile_incomplete` (C1).
///
/// A local row carries `compensation_plan == NULL` by construction (`local_journaled_write`
/// never sets one — the connector vocabulary of op/model/id does not apply), so the second
/// half of this predicate is what distinguishes a genuine local row from a NULL-plan
/// CONNECTOR row (a create whose plan write-back never landed, M3c): only a `tool_name` in
/// the closed allowlist routes here.
pub fn is_local_row(row: &ActionJournalRow) -> bool {
    row.compensation_plan.is_none() && local_table_for(&row.tool_name).is_some()
}

/// Local-row refusal rules (own set — deliberately DROPS the connector's NULL-plan rule,
/// since every local row is legitimately NULL-plan). Refuses on: already-undone, `final`
/// compensability, retention expired, session mismatch (M1).
fn local_refusal_reason(row: &ActionJournalRow, session_id: &str) -> Option<String> {
    if row.session_id != session_id {
        // Deliberately the SAME message shape as "not found" territory — a caller must not
        // be able to distinguish "wrong session" from "doesn't exist" (M1 boundary).
        return Some("không tìm thấy hành động này trong phiên hiện tại".to_string());
    }
    if row.undo_status == "undone" {
        return Some("hành động này đã được hoàn tác trước đó".to_string());
    }
    if row.compensability == "final" {
        return Some("hành động này không thể hoàn tác (final)".to_string());
    }
    if retention_expired(&row.retention_expires_at) {
        return Some("bản ghi hoàn tác đã hết hạn lưu trữ".to_string());
    }
    None
}

fn retention_expired(retention_expires_at: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(retention_expires_at) {
        Ok(exp) => exp < chrono::Utc::now(),
        Err(_) => true, // fail-closed
    }
}

/// Persist the `refused` terminal state and build the matching outcome — the shape repeated
/// at every refusal point in `local_attempt_undo`. Callers roll back their own transaction
/// (if one is open) BEFORE calling this; it only touches the journal row.
async fn refuse(db: &DbHandle, row_id: &str, reason: impl Into<String>) -> Result<UndoOutcome> {
    journal::advance_undo_status(db, row_id, "refused").await?;
    Ok(UndoOutcome::Refused(reason.into()))
}

/// Undo one local-tool journal row. NO `ConnectorExecutor` involved — the record lives in
/// this process's own SQLite, so undo is a direct, C10-guarded UPDATE against `pre_state`.
///
/// Sequence: session-scope + local refusal rules → resolve op kind/table → C10-guarded
/// restore (via `local_snapshot`, `rows_affected()==0` => refused, never a separate SELECT)
/// → `undone`.
pub async fn local_attempt_undo(
    db: &DbHandle,
    row: &ActionJournalRow,
    session_id: &str,
) -> Result<UndoOutcome> {
    if let Some(reason) = local_refusal_reason(row, session_id) {
        return refuse(db, &row.id, reason).await;
    }

    let table = match local_table_for(&row.tool_name) {
        Some(t) => t,
        // Unreachable in practice — `is_local_row` already checked this — but fail closed
        // rather than panic if ever called directly.
        None => {
            return refuse(
                db,
                &row.id,
                "không xác định được bảng dữ liệu cục bộ cho hành động này",
            )
            .await
        }
    };
    let kind = match op_kind(&row.tool_name) {
        Some(k) => k,
        None => {
            return refuse(
                db,
                &row.id,
                "không xác định được kiểu thao tác cục bộ cho hành động này",
            )
            .await
        }
    };

    // A FIRED reminder's real-world side effect (the notification) already happened and
    // cannot be un-sent — refuse as `final` regardless of what the journal recorded at write
    // time, since the reminder may have fired at any point AFTER the write and BEFORE this
    // undo request (a live check, not a journaled compensability).
    if table == LocalTable::Reminders
        && local_snapshot::reminder_is_fired(db, &row.correlation_ref).await?
    {
        return refuse(db, &row.id, "nhắc nhở đã được gửi — không thể hoàn tác (final)").await;
    }

    journal::advance_undo_status(db, &row.id, "undo_requested").await?;
    journal::increment_undo_attempt(db, &row.id).await?;

    // The C10 baseline for a local row is its OWN post_state_version (the row's updated_at
    // as of the forward write's completion) — there is no third-party version token.
    let expected_updated_at = match row.post_state_version.as_deref() {
        Some(v) => v,
        None => {
            journal::advance_undo_status(db, &row.id, "stuck").await?;
            return Ok(UndoOutcome::Stuck(
                "không có phiên bản ghi nhận để đối chiếu — cần xử lý thủ công".to_string(),
            ));
        }
    };

    journal::advance_undo_status(db, &row.id, "compensating").await?;

    let mut tx = db.pool().begin().await?;
    let affected = match kind {
        LocalOpKind::Create => {
            local_snapshot::soft_delete_row(&mut tx, table, &row.correlation_ref, expected_updated_at)
                .await?
        }
        LocalOpKind::Delete => {
            local_snapshot::clear_deleted_at(&mut tx, table, &row.correlation_ref, expected_updated_at)
                .await?
        }
        LocalOpKind::Update => {
            let pre: Value = match row.pre_state.as_deref().and_then(|s| serde_json::from_str(s).ok()) {
                Some(v) => v,
                None => {
                    tx.rollback().await.ok();
                    return refuse(db, &row.id, "không có pre_state để khôi phục").await;
                }
            };
            local_snapshot::restore_row(&mut tx, table, &row.correlation_ref, &pre, expected_updated_at)
                .await?
        }
    };

    if affected == 0 {
        tx.rollback().await.ok();
        return refuse(
            db,
            &row.id,
            "bản ghi đã bị thay đổi kể từ khi ghi nhận — từ chối hoàn tác",
        )
        .await;
    }
    tx.commit().await?;

    journal::set_readback(db, &row.id, "match", None).await?;
    journal::advance_undo_status(db, &row.id, "undone").await?;
    Ok(UndoOutcome::Undone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::queries::local_snapshot::{local_journaled_write, LocalMutation};

    async fn db() -> (DbHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn create_then_undo_soft_deletes_row() {
        let (db, _d) = db().await;
        let (row, _v) = local_journaled_write(
            &db,
            LocalMutation::TaskCreate {
                id: "task-c1",
                title: "New task",
                description: None,
                priority: "medium",
                due_at: None,
            },
            "sess-1",
            "task_create",
            "ReversibleWrite",
            "{}",
            30,
        )
        .await
        .unwrap()
        .expect("target exists");
        let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();
        assert!(is_local_row(&row));

        let outcome = local_attempt_undo(&db, &row, "sess-1").await.unwrap();
        assert_eq!(outcome, UndoOutcome::Undone);

        let active = haily_db::queries::tasks::active(&db).await.unwrap();
        assert!(
            active.iter().all(|t| t.id != "task-c1"),
            "created task must be soft-deleted after undo"
        );
    }

    #[tokio::test]
    async fn update_then_undo_restores_previous_fields() {
        let (db, _d) = db().await;
        haily_db::queries::tasks::insert(&db, "Original", None, "low", None, None)
            .await
            .unwrap();
        let task = haily_db::queries::tasks::active(&db).await.unwrap().remove(0);

        let (row, _v) = local_journaled_write(
            &db,
            LocalMutation::TaskComplete { id: &task.id },
            "sess-1",
            "task_complete",
            "ReversibleWrite",
            "{}",
            30,
        )
        .await
        .unwrap()
        .expect("target exists");
        let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();
        assert_eq!(row.correlation_ref, task.id);

        let outcome = local_attempt_undo(&db, &row, "sess-1").await.unwrap();
        assert_eq!(outcome, UndoOutcome::Undone);

        let active = haily_db::queries::tasks::active(&db).await.unwrap();
        assert!(
            active.iter().any(|t| t.id == task.id && t.status == "todo"),
            "undo must restore the pre-complete status"
        );

        // Regression guard for the pre-existing FTS5 trigger corruption (migration 0015):
        // this is the second UPDATE (complete, then undo) that used to hit SQLITE_CORRUPT
        // by issuing an unconditional 'delete' for a rowid already absent from the FTS index.
        // A failed/corrupted index would make this search return nothing, not error out.
        let found = haily_db::queries::tasks::search_fts(&db, "Original", 10)
            .await
            .unwrap();
        assert!(
            found.iter().any(|t| t.id == task.id),
            "FTS index must remain queryable and find the task after complete+undo (no corruption)"
        );
    }

    #[tokio::test]
    async fn delete_then_undo_restores_visibility() {
        let (db, _d) = db().await;
        haily_db::queries::tasks::insert(&db, "To delete", None, "low", None, None)
            .await
            .unwrap();
        let task = haily_db::queries::tasks::active(&db).await.unwrap().remove(0);

        let (row, _v) = local_journaled_write(
            &db,
            LocalMutation::TaskDelete { id: &task.id },
            "sess-1",
            "task_delete",
            "IrreversibleWrite",
            "{}",
            30,
        )
        .await
        .unwrap()
        .expect("target exists");
        let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();

        let outcome = local_attempt_undo(&db, &row, "sess-1").await.unwrap();
        assert_eq!(outcome, UndoOutcome::Undone);

        let active = haily_db::queries::tasks::active(&db).await.unwrap();
        assert!(
            active.iter().any(|t| t.id == task.id),
            "undo must restore the soft-deleted task to active"
        );
    }

    #[tokio::test]
    async fn c10_refuses_on_external_edit_between_write_and_undo() {
        let (db, _d) = db().await;
        haily_db::queries::tasks::insert(&db, "Racy", None, "low", None, None)
            .await
            .unwrap();
        let task = haily_db::queries::tasks::active(&db).await.unwrap().remove(0);

        let (row, _v) = local_journaled_write(
            &db,
            LocalMutation::TaskComplete { id: &task.id },
            "sess-1",
            "task_complete",
            "ReversibleWrite",
            "{}",
            30,
        )
        .await
        .unwrap()
        .expect("target exists");
        let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();

        // External edit changes updated_at without going through the journal (a plain public
        // query, not raw SQL — `haily-tools` has no direct `sqlx` dependency by design).
        haily_db::queries::tasks::update_status(&db, &task.id, "cancelled")
            .await
            .unwrap();

        let outcome = local_attempt_undo(&db, &row, "sess-1").await.unwrap();
        assert!(
            matches!(outcome, UndoOutcome::Refused(_)),
            "must refuse via rows_affected==0, not blindly overwrite: {outcome:?}"
        );
        let after = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();
        assert_eq!(after.undo_status, "refused");
    }

    #[tokio::test]
    async fn session_mismatch_refuses() {
        let (db, _d) = db().await;
        let (row, _v) = local_journaled_write(
            &db,
            LocalMutation::TaskCreate {
                id: "task-sec",
                title: "Sensitive",
                description: None,
                priority: "medium",
                due_at: None,
            },
            "sess-owner",
            "task_create",
            "ReversibleWrite",
            "{}",
            30,
        )
        .await
        .unwrap()
        .expect("target exists");
        let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();

        let outcome = local_attempt_undo(&db, &row, "sess-attacker").await.unwrap();
        assert!(
            matches!(outcome, UndoOutcome::Refused(_)),
            "cross-session undo must be refused: {outcome:?}"
        );
        let active = haily_db::queries::tasks::active(&db).await.unwrap();
        assert!(
            active.iter().any(|t| t.id == "task-sec"),
            "the row must remain untouched after a cross-session refusal"
        );
    }

    #[tokio::test]
    async fn note_with_wikilinks_then_undo_restores_pre_wikilink_state() {
        // M3b: `note_save` does insert THEN a second write (set_wikilinks) that bumps
        // updated_at again — `local_journaled_write` must capture `post_state_version`
        // AFTER that LAST write, or this undo's C10 guard would refuse on our own second
        // write. Undoing a CREATE with wikilinks must soft-delete the note cleanly.
        let (db, _d) = db().await;
        let (row, _v) = local_journaled_write(
            &db,
            LocalMutation::NoteSave {
                id: "note-wl-1",
                title: "Linked note",
                content: "see [[Other Note]]",
                tags: None,
                wikilinks: Some("Other Note"),
            },
            "sess-1",
            "note_save",
            "ReversibleWrite",
            "{}",
            30,
        )
        .await
        .unwrap()
        .expect("target exists");
        let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();
        assert!(is_local_row(&row));

        let outcome = local_attempt_undo(&db, &row, "sess-1").await.unwrap();
        assert_eq!(
            outcome,
            UndoOutcome::Undone,
            "post_state_version must reflect the LAST write (post-wikilinks), not the insert"
        );

        let note = haily_db::queries::notes::get(&db, "note-wl-1").await.unwrap();
        assert!(note.is_none(), "undo of a create must soft-delete the note");
    }

    #[tokio::test]
    async fn notes_pre_state_never_contains_embedding_key() {
        // Constraint 10: the `embedding` BLOB column must never appear in a note's
        // pre_state snapshot (regenerable, and a BLOB has no meaningful JSON shape).
        let (db, _d) = db().await;
        haily_db::queries::notes::insert(&db, "T", "content", None, None, Some(&[1, 2, 3]))
            .await
            .unwrap();
        let note = haily_db::queries::notes::search_fts(&db, "content", 10)
            .await
            .unwrap()
            .remove(0);

        let (row, _v) = local_journaled_write(
            &db,
            LocalMutation::NoteUpdate {
                id: &note.id,
                title: "T2",
                content: "updated content",
            },
            "sess-1",
            "note_update",
            "ReversibleWrite",
            "{}",
            30,
        )
        .await
        .unwrap()
        .expect("target exists");
        let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();
        let pre_state = row.pre_state.expect("update captures a pre_state");
        assert!(
            !pre_state.contains("embedding"),
            "embedding BLOB must never appear in pre_state: {pre_state}"
        );
    }

    #[tokio::test]
    async fn fired_reminder_undo_refuses_as_final() {
        let (db, _d) = db().await;
        let (row, _v) = local_journaled_write(
            &db,
            LocalMutation::ReminderAdd {
                id: "rem-fired-1",
                title: "Take medicine",
                fire_at: "2026-07-01T08:00:00Z",
                recurrence: None,
                session_id: "sess-1",
            },
            "sess-1",
            "reminder_add",
            "ReversibleWrite",
            "{}",
            30,
        )
        .await
        .unwrap()
        .expect("target exists");

        // Simulate the scheduler firing the reminder (real-world notification already sent).
        haily_db::queries::reminders::mark_fired(&db, "rem-fired-1", "2026-07-01T08:00:05Z")
            .await
            .unwrap();

        // Undo the CREATE (mint of the reminder) — must refuse now that it has fired, even
        // though the journal recorded it as ordinary `compensatable` at write time.
        let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();
        let outcome = local_attempt_undo(&db, &row, "sess-1").await.unwrap();
        assert!(
            matches!(outcome, UndoOutcome::Refused(_)),
            "a fired reminder's undo must be refused as final: {outcome:?}"
        );
        let active = haily_db::queries::reminders::list_all(&db).await.unwrap();
        assert!(
            active.iter().any(|r| r.id == "rem-fired-1"),
            "the fired reminder must remain untouched after refusal"
        );
    }

    #[tokio::test]
    async fn null_plan_connector_tool_is_not_local() {
        // M3c: a NULL-plan row whose tool_name is a CONNECTOR op (not in the local
        // allowlist) must NOT be classified local, even though compensation_plan is None —
        // exactly the "lost plan write-back" crash scenario.
        let row = ActionJournalRow {
            id: "j1".into(),
            session_id: "sess-1".into(),
            tool_name: "odoo_contact_create".into(),
            tool_tier: "IrreversibleWrite".into(),
            compensability: "compensatable".into(),
            idempotency_key: "idem-1".into(),
            correlation_ref: "corr-1".into(),
            request_params: "{}".into(),
            pre_state: None,
            pre_state_version: None,
            post_state: None,
            post_state_version: None,
            readback_status: "pending".into(),
            compensation_plan: None,
            undo_status: "not_requested".into(),
            undo_attempts: 0,
            created_at: "2026-07-03T00:00:00Z".into(),
            undone_at: None,
            retention_expires_at: "2026-08-02T00:00:00Z".into(),
        };
        assert!(
            !is_local_row(&row),
            "a NULL-plan CONNECTOR tool_name must route to the connector path, not local"
        );
    }
}
