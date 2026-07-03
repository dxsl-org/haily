/// F1 regression tests: `create_session` must persist under the caller-supplied id so
/// dependent rows (e.g. `work_items.session_id`, FK-constrained) resolve correctly.
use haily_db::{queries::sessions, DbHandle};

async fn setup() -> (DbHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = DbHandle::init(&db_path).await.unwrap();
    (db, dir)
}

#[tokio::test]
async fn create_session_persists_under_caller_id() {
    let (db, _dir) = setup().await;
    let caller_id = uuid::Uuid::new_v4().to_string();

    let created = sessions::create_session(&db, &caller_id, "cli", None)
        .await
        .unwrap();
    assert_eq!(
        created.id, caller_id,
        "session row must use the caller's id, not a fresh one"
    );

    let fetched = sessions::get_session(&db, &caller_id).await.unwrap();
    assert!(
        fetched.is_some(),
        "second lookup by the same id must find the row (no new row)"
    );
    assert_eq!(fetched.unwrap().id, caller_id);
}

#[tokio::test]
async fn work_item_insert_succeeds_once_session_exists_under_same_id() {
    // Reproduces the F1 bug: work_items.session_id has an FK to sessions(id).
    // Before the fix, run_turn created the session under a fresh UUID while
    // inserting work_items under req.session_id — the FK violated silently
    // because the caller swallowed the error with `if let Ok(wi)`.
    let (db, _dir) = setup().await;
    let session_id = uuid::Uuid::new_v4().to_string();

    sessions::create_session(&db, &session_id, "cli", None)
        .await
        .unwrap();

    let wi = haily_db::queries::work_items::create(&db, &session_id, "tool-calling turn")
        .await
        .expect("work item insert must succeed once the session row exists under the same id");
    assert_eq!(wi.session_id, session_id);
}

#[tokio::test]
async fn work_item_insert_fails_fk_when_session_row_absent() {
    // Guards the inverse: an id with no session row must still violate the FK.
    // This is what silently happened in production before F1 was fixed.
    let (db, _dir) = setup().await;
    let orphan_session_id = uuid::Uuid::new_v4().to_string();

    let result = haily_db::queries::work_items::create(&db, &orphan_session_id, "orphan").await;
    assert!(
        result.is_err(),
        "FK must reject a work_item under a nonexistent session"
    );
}

/// Replicates `run_turn`'s get-or-create idiom (agent.rs) directly against the DB:
/// first turn creates the row, every subsequent turn under the same session_id only
/// touches it. Two turns must leave exactly one `sessions` row — the CLI manual
/// success criterion ("one CLI conversation produces exactly 1 sessions row").
#[tokio::test]
async fn repeated_turns_under_same_session_id_leave_exactly_one_row() {
    let (db, _dir) = setup().await;
    let session_id = uuid::Uuid::new_v4().to_string();

    for _ in 0..3 {
        if sessions::get_session(&db, &session_id)
            .await
            .unwrap()
            .is_none()
        {
            sessions::create_session(&db, &session_id, "cli", None)
                .await
                .unwrap();
        } else {
            sessions::touch_session(&db, &session_id).await.unwrap();
        }
    }

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        count.0, 1,
        "three turns under the same session_id must leave exactly one row"
    );
}
