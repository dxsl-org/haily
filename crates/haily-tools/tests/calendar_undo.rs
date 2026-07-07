//! Integration tests: calendar occurrence-vs-series undo + exceptions (Phase 13b,
//! assistant-depth).
//!
//! Three scopes are covered end to end: (1) a non-recurring event's series-delete
//! (identical shape to `task_delete`), (2) a recurring event's occurrence-delete (records
//! an exception `upcoming` subtracts, leaving sibling occurrences untouched), and (3) a
//! recurring event's series-delete (soft-deletes the row, removing every occurrence at
//! once). `local_attempt_undo` takes `&KmsHandle` per Phase 12's signature even though
//! calendar undo never touches it — see the phase's Architecture note.

use haily_db::queries::calendar::{self, NewCalendarEvent};
use haily_db::queries::journal;
use haily_db::queries::local_snapshot::{local_journaled_write, LocalMutation};
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

async fn seed_event(
    db: &DbHandle,
    title: &str,
    start_at: &str,
    end_at: &str,
    recurrence: Option<&str>,
) -> calendar::CalendarEvent {
    calendar::insert(
        db,
        NewCalendarEvent {
            title,
            description: None,
            location: None,
            start_at,
            end_at,
            all_day: false,
            recurrence,
        },
    )
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// Non-recurring event: series-delete is the only meaningful scope.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_recurring_series_delete_then_undo_restores_visibility() {
    let (db, kms, _d) = db().await;
    let event = seed_event(
        &db,
        "1:1",
        "2026-07-10T09:00:00+00:00",
        "2026-07-10T09:30:00+00:00",
        None,
    )
    .await;

    let (row, _v) = local_journaled_write(
        &db,
        LocalMutation::CalendarDeleteSeries { id: &event.id },
        "sess-1",
        "calendar_delete_series",
        "ReversibleWrite",
        "{}",
        None,
        30,
    )
    .await
    .unwrap()
    .expect("target exists");
    let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();
    assert!(is_local_row(&row), "calendar_delete_series must route through the local path");
    assert!(calendar::get(&db, &event.id).await.unwrap().is_none());

    let outcome = local_attempt_undo(&db, &kms, &row, "sess-1").await.unwrap();
    assert_eq!(outcome, UndoOutcome::Undone);

    let restored = calendar::get(&db, &event.id)
        .await
        .unwrap()
        .expect("undo must restore the soft-deleted event");
    assert_eq!(restored.title, "1:1");
}

#[tokio::test]
async fn series_delete_missing_target_does_not_journal() {
    let (db, _kms, _d) = db().await;
    let outcome = local_journaled_write(
        &db,
        LocalMutation::CalendarDeleteSeries { id: "no-such-id" },
        "sess-1",
        "calendar_delete_series",
        "ReversibleWrite",
        "{}",
        None,
        30,
    )
    .await
    .unwrap();
    assert!(outcome.is_none(), "deleting a nonexistent event must not mint a journal row");
}

// ---------------------------------------------------------------------------
// Recurring event: occurrence-delete leaves the series row (and every sibling
// occurrence) untouched — only the ONE targeted occurrence disappears from `upcoming`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn occurrence_delete_then_undo_restores_just_that_occurrence() {
    let (db, kms, _d) = db().await;
    let event = seed_event(
        &db,
        "Daily standup",
        "2026-07-06T09:00:00+00:00",
        "2026-07-06T09:15:00+00:00",
        Some("daily"),
    )
    .await;
    let from = "2026-07-06T00:00:00+00:00";
    let to = "2026-07-09T23:59:59+00:00";

    let before = calendar::upcoming(&db, from, to).await.unwrap();
    assert_eq!(before.len(), 4, "4 daily occurrences across the window before any delete");

    let target_occurrence = "2026-07-07T09:00:00+00:00";
    let (row, _v) = local_journaled_write(
        &db,
        LocalMutation::CalendarDeleteOccurrence {
            event_id: &event.id,
            occurrence_start: target_occurrence,
        },
        "sess-1",
        "calendar_delete_occurrence",
        "ReversibleWrite",
        "{}",
        None,
        30,
    )
    .await
    .unwrap()
    .expect("target exists");
    let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();
    assert!(is_local_row(&row), "calendar_delete_occurrence must route through the local path");

    let after_delete = calendar::upcoming(&db, from, to).await.unwrap();
    assert_eq!(after_delete.len(), 3, "only the targeted occurrence is removed");
    assert!(
        after_delete.iter().all(|e| e.start_at != target_occurrence),
        "the deleted occurrence must not surface"
    );

    let outcome = local_attempt_undo(&db, &kms, &row, "sess-1").await.unwrap();
    assert_eq!(outcome, UndoOutcome::Undone);

    let after_undo = calendar::upcoming(&db, from, to).await.unwrap();
    assert_eq!(after_undo.len(), 4, "undo must restore the occurrence");
    assert!(
        after_undo.iter().any(|e| e.start_at == target_occurrence),
        "the restored occurrence must surface again"
    );
}

#[tokio::test]
async fn duplicate_occurrence_delete_is_a_noop_and_does_not_double_journal() {
    let (db, _kms, _d) = db().await;
    let event = seed_event(
        &db,
        "Weekly sync",
        "2026-07-06T09:00:00+00:00",
        "2026-07-06T09:15:00+00:00",
        Some("daily"),
    )
    .await;
    let occurrence_start = "2026-07-07T09:00:00+00:00";

    let first = local_journaled_write(
        &db,
        LocalMutation::CalendarDeleteOccurrence {
            event_id: &event.id,
            occurrence_start,
        },
        "sess-1",
        "calendar_delete_occurrence",
        "ReversibleWrite",
        "{}",
        None,
        30,
    )
    .await
    .unwrap();
    assert!(first.is_some(), "the first occurrence-delete must succeed");

    let second = local_journaled_write(
        &db,
        LocalMutation::CalendarDeleteOccurrence {
            event_id: &event.id,
            occurrence_start,
        },
        "sess-1",
        "calendar_delete_occurrence",
        "ReversibleWrite",
        "{}",
        None,
        30,
    )
    .await
    .unwrap();
    assert!(
        second.is_none(),
        "a duplicate exception (UNIQUE conflict, rows_affected==0) must not mint a second journal row"
    );
}

#[tokio::test]
async fn occurrence_undo_replay_refuses() {
    // C10 parity: replaying undo against the SAME pre-undo snapshot must still be
    // refused — caught by `remove_exception_tx`'s `rows_affected()==0` guard (the
    // exception row is already gone after the first, successful undo).
    let (db, kms, _d) = db().await;
    let event = seed_event(
        &db,
        "Replay target",
        "2026-07-06T09:00:00+00:00",
        "2026-07-06T09:15:00+00:00",
        Some("daily"),
    )
    .await;
    let occurrence_start = "2026-07-07T09:00:00+00:00";

    let (row, _v) = local_journaled_write(
        &db,
        LocalMutation::CalendarDeleteOccurrence {
            event_id: &event.id,
            occurrence_start,
        },
        "sess-1",
        "calendar_delete_occurrence",
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
        "a replay against the pre-undo snapshot must refuse, not double-remove: {outcome2:?}"
    );
}

#[tokio::test]
async fn occurrence_undo_session_mismatch_refuses() {
    let (db, kms, _d) = db().await;
    let event = seed_event(
        &db,
        "Sensitive",
        "2026-07-06T09:00:00+00:00",
        "2026-07-06T09:15:00+00:00",
        Some("daily"),
    )
    .await;
    let occurrence_start = "2026-07-07T09:00:00+00:00";

    let (row, _v) = local_journaled_write(
        &db,
        LocalMutation::CalendarDeleteOccurrence {
            event_id: &event.id,
            occurrence_start,
        },
        "sess-owner",
        "calendar_delete_occurrence",
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

    let from = "2026-07-06T00:00:00+00:00";
    let to = "2026-07-09T23:59:59+00:00";
    let occurrences = calendar::upcoming(&db, from, to).await.unwrap();
    assert!(
        occurrences.iter().all(|e| e.start_at != occurrence_start),
        "the exception must remain in place after a cross-session refusal"
    );
}

// ---------------------------------------------------------------------------
// Recurring event, SERIES scope: soft-deletes the row, removing every occurrence at
// once — contrast with the occurrence-scope tests above.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recurring_series_delete_then_undo_restores_every_occurrence() {
    let (db, kms, _d) = db().await;
    let event = seed_event(
        &db,
        "Recurring series",
        "2026-07-06T09:00:00+00:00",
        "2026-07-06T09:15:00+00:00",
        Some("daily"),
    )
    .await;
    let from = "2026-07-06T00:00:00+00:00";
    let to = "2026-07-09T23:59:59+00:00";

    assert_eq!(calendar::upcoming(&db, from, to).await.unwrap().len(), 4);

    let (row, _v) = local_journaled_write(
        &db,
        LocalMutation::CalendarDeleteSeries { id: &event.id },
        "sess-1",
        "calendar_delete_series",
        "ReversibleWrite",
        "{}",
        None,
        30,
    )
    .await
    .unwrap()
    .expect("target exists");
    let row = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();

    assert!(
        calendar::upcoming(&db, from, to).await.unwrap().is_empty(),
        "a series-deleted event must contribute no occurrences at all"
    );

    let outcome = local_attempt_undo(&db, &kms, &row, "sess-1").await.unwrap();
    assert_eq!(outcome, UndoOutcome::Undone);

    assert_eq!(
        calendar::upcoming(&db, from, to).await.unwrap().len(),
        4,
        "undo of a series-delete must restore every occurrence"
    );
}
