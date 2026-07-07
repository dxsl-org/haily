//! Integration tests: work_items soft-delete + journal + undo (Phase 11, assistant-depth).
//!
//! work_items has exactly ONE tool-driven mutation — `work_item_delete`
//! (create/start/checkpoint/complete/fail/mark_interrupted all run internally from
//! `agent.rs`, never through a `Tool`, so they carry no journal coverage by design —
//! see `LocalMutation::WorkItemDelete`'s doc comment). These tests therefore cover the
//! Delete path end to end: undo restores the row, a replay against a stale snapshot is
//! refused (C10), and a cross-session undo attempt is refused (M1).

use haily_db::queries::local_snapshot::{local_journaled_write, LocalMutation};
use haily_db::queries::{journal, sessions, work_items};
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_tools::journal_undo::{is_local_row, local_attempt_undo, UndoOutcome};
use std::sync::Arc;

async fn db() -> (DbHandle, Arc<KmsHandle>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
    let kms = Arc::new(KmsHandle::init(db.clone(), dir.path()).await.unwrap());
    (db, kms, dir)
}

/// A work item needs a valid `session_id` FK — mint a throwaway session for it.
async fn seed_work_item(db: &DbHandle, title: &str) -> work_items::WorkItem {
    let session_id = uuid::Uuid::new_v4().to_string();
    sessions::create_session(db, &session_id, "test-adapter", None)
        .await
        .unwrap();
    work_items::create(db, &session_id, title).await.unwrap()
}

#[tokio::test]
async fn delete_then_undo_restores_visibility() {
    let (db, kms, _d) = db().await;
    let item = seed_work_item(&db, "To delete").await;

    let (row, _v) = local_journaled_write(
        &db,
        LocalMutation::WorkItemDelete { id: &item.id },
        "sess-1",
        "work_item_delete",
        "ReversibleWrite",
        "{}",
        None,
        30,
    )
    .await
    .unwrap()
    .expect("target exists");
    let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();
    assert!(is_local_row(&row), "work_item_delete must route through the local path");

    assert!(
        work_items::get(&db, &item.id).await.unwrap().is_none(),
        "a soft-deleted work item must be invisible to get()"
    );

    let outcome = local_attempt_undo(&db, &kms, &row, "sess-1").await.unwrap();
    assert_eq!(outcome, UndoOutcome::Undone);

    let restored = work_items::get(&db, &item.id)
        .await
        .unwrap()
        .expect("undo must restore the soft-deleted work item");
    assert_eq!(restored.title, "To delete");
    assert!(restored.deleted_at.is_none());
}

#[tokio::test]
async fn missing_target_does_not_journal() {
    let (db, _kms, _d) = db().await;
    let outcome = local_journaled_write(
        &db,
        LocalMutation::WorkItemDelete { id: "no-such-id" },
        "sess-1",
        "work_item_delete",
        "ReversibleWrite",
        "{}",
        None,
        30,
    )
    .await
    .unwrap();
    assert!(
        outcome.is_none(),
        "deleting a nonexistent work item must not mint a journal row"
    );
}

#[tokio::test]
async fn delete_then_double_undo_refuses_the_replay() {
    // C10 parity with the other local tables: replaying undo against the SAME
    // pre-undo snapshot (its `undo_status` still reads "not_requested", so the
    // ordinary already-undone shortcut never fires) must still be refused — caught
    // by `clear_deleted_at`'s `rows_affected()==0` guard against the LIVE
    // `updated_at` (bumped by the first, successful undo).
    let (db, kms, _d) = db().await;
    let item = seed_work_item(&db, "Replay target").await;

    let (row, _v) = local_journaled_write(
        &db,
        LocalMutation::WorkItemDelete { id: &item.id },
        "sess-1",
        "work_item_delete",
        "ReversibleWrite",
        "{}",
        None,
        30,
    )
    .await
    .unwrap()
    .expect("target exists");
    let stale_row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();

    let outcome1 = local_attempt_undo(&db, &kms, &stale_row, "sess-1").await.unwrap();
    assert_eq!(outcome1, UndoOutcome::Undone, "first undo must succeed normally");

    let outcome2 = local_attempt_undo(&db, &kms, &stale_row, "sess-1").await.unwrap();
    assert!(
        matches!(outcome2, UndoOutcome::Refused(_)),
        "a replay against the pre-undo snapshot must refuse via the C10 guard, \
         not double-restore: {outcome2:?}"
    );
}

#[tokio::test]
async fn session_mismatch_refuses() {
    let (db, kms, _d) = db().await;
    let item = seed_work_item(&db, "Sensitive").await;

    let (row, _v) = local_journaled_write(
        &db,
        LocalMutation::WorkItemDelete { id: &item.id },
        "sess-owner",
        "work_item_delete",
        "ReversibleWrite",
        "{}",
        None,
        30,
    )
    .await
    .unwrap()
    .expect("target exists");
    let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();

    let outcome = local_attempt_undo(&db, &kms, &row, "sess-attacker").await.unwrap();
    assert!(
        matches!(outcome, UndoOutcome::Refused(_)),
        "cross-session undo must be refused: {outcome:?}"
    );
    assert!(
        work_items::get(&db, &item.id).await.unwrap().is_none(),
        "the row must remain deleted after a cross-session refusal"
    );
}
