//! DB-layer tests for the action journal: append-only triggers, idempotency, the undo
//! state machine, incomplete-row selection, and retention purge. The tool-layer behaviors
//! (redaction, tag-strip, reconciliation via read-back, undo refusal/retry) live in
//! haily-tools/haily-core tests where the executor mock is available.
use haily_db::{
    queries::{journal, sessions},
    DbHandle,
};

async fn setup() -> (DbHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = DbHandle::init(&db_path).await.unwrap();
    (db, dir)
}

async fn make_session(db: &DbHandle) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sessions::create_session(db, &id, "test-adapter", None)
        .await
        .unwrap()
        .id
}

fn new_action<'a>(session_id: &'a str, key: &'a str) -> journal::NewAction<'a> {
    journal::NewAction {
        session_id,
        tool_name: "odoo_create",
        tool_tier: "IrreversibleWrite",
        compensability: "compensatable",
        idempotency_key: key,
        correlation_ref: "corr-123",
        request_params: r#"{"model":"res.partner","cred_ref":"odoo.api_key"}"#,
        pre_state: Some(r#"{"id":null}"#),
        pre_state_version: Some("2026-07-03 10:00:00"),
        compensation_plan: Some(r#"{"op":"unlink","id":42}"#),
        turn_id: None,
        retention_days: 30,
    }
}

#[tokio::test]
async fn migration_0012_applies_and_insert_roundtrips() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;
    let row = journal::insert(&db, new_action(&sid, "op-1"))
        .await
        .unwrap();
    assert_eq!(row.readback_status, "pending");
    assert_eq!(row.undo_status, "not_requested");
    assert_eq!(row.undo_attempts, 0);
    assert!(
        row.compensation_plan.is_some(),
        "outbox: plan present at insert"
    );
    assert!(
        row.pre_state.is_some(),
        "outbox: pre_state present at insert"
    );
}

#[tokio::test]
async fn no_update_of_evidentiary_columns() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;
    let row = journal::insert(&db, new_action(&sid, "op-ev"))
        .await
        .unwrap();

    // A direct rewrite of an evidentiary column must be ABORTed by the trigger.
    let err = sqlx::query("UPDATE action_journal SET request_params = 'tampered' WHERE id = ?")
        .bind(&row.id)
        .execute(db.pool())
        .await;
    assert!(err.is_err(), "rewriting request_params must abort");

    let err2 = sqlx::query("UPDATE action_journal SET pre_state = 'tampered' WHERE id = ?")
        .bind(&row.id)
        .execute(db.pool())
        .await;
    assert!(err2.is_err(), "rewriting pre_state must abort");
}

#[tokio::test]
async fn undo_status_update_allowed() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;
    let row = journal::insert(&db, new_action(&sid, "op-proc"))
        .await
        .unwrap();

    // Processing columns stay mutable — the state machine + read-back must advance.
    journal::advance_undo_status(&db, &row.id, "undo_requested")
        .await
        .unwrap();
    journal::set_readback(&db, &row.id, "match", Some(r#"{"id":42}"#))
        .await
        .unwrap();
    let n = journal::increment_undo_attempt(&db, &row.id).await.unwrap();
    assert_eq!(n, 1);

    let after = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();
    assert_eq!(after.undo_status, "undo_requested");
    assert_eq!(after.readback_status, "match");
    assert_eq!(after.undo_attempts, 1);

    journal::advance_undo_status(&db, &row.id, "undone")
        .await
        .unwrap();
    let done = journal::get_by_id(&db, &row.id).await.unwrap().unwrap();
    assert_eq!(done.undo_status, "undone");
    assert!(done.undone_at.is_some(), "undone_at set on terminal undone");
}

#[tokio::test]
async fn delete_allowed_for_purge() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;
    let row = journal::insert(&db, new_action(&sid, "op-del"))
        .await
        .unwrap();

    // No blanket DELETE trigger — a raw delete must succeed so purge + migrations work.
    sqlx::query("DELETE FROM action_journal WHERE id = ?")
        .bind(&row.id)
        .execute(db.pool())
        .await
        .expect("DELETE must succeed (no blanket DELETE trigger)");
    assert!(journal::get_by_id(&db, &row.id).await.unwrap().is_none());
}

#[tokio::test]
async fn idempotency_key_unique() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;
    journal::insert(&db, new_action(&sid, "dup-key"))
        .await
        .unwrap();
    let second = journal::insert(&db, new_action(&sid, "dup-key")).await;
    assert!(second.is_err(), "duplicate idempotency_key must conflict");
}

#[tokio::test]
async fn list_incomplete_selects_only_stale_pending() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let pending = journal::insert(&db, new_action(&sid, "op-pending"))
        .await
        .unwrap();

    let resolved = journal::insert(&db, new_action(&sid, "op-resolved"))
        .await
        .unwrap();
    journal::set_readback(&db, &resolved.id, "match", None)
        .await
        .unwrap();

    // grace_secs = -1 → cutoff is in the FUTURE, so the just-inserted pending row counts.
    let incomplete = journal::list_incomplete(&db, -1).await.unwrap();
    let ids: Vec<&str> = incomplete.iter().map(|r| r.id.as_str()).collect();
    assert!(
        ids.contains(&pending.id.as_str()),
        "pending orphan must be listed"
    );
    assert!(
        !ids.contains(&resolved.id.as_str()),
        "resolved row must not be listed"
    );
}

#[tokio::test]
async fn outbox_row_survives_mid_write_crash() {
    // Outbox invariant: the journal row (compensation_plan + pre_state) is inserted BEFORE
    // the external call. Simulate a crash right after that insert (no set_readback ever
    // runs) by re-opening the DB and confirming the row is durably present with its plan.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("outbox.db");
    let row_id = {
        let db = DbHandle::init(&db_path).await.unwrap();
        let sid = make_session(&db).await;
        let row = journal::insert(&db, new_action(&sid, "op-crash"))
            .await
            .unwrap();
        // No external call, no set_readback — this is the crash point.
        row.id
    };
    // Re-open (fresh handle == process restart).
    let db2 = DbHandle::init(&db_path).await.unwrap();
    let recovered = journal::get_by_id(&db2, &row_id).await.unwrap().unwrap();
    assert_eq!(
        recovered.readback_status, "pending",
        "orphan left pending for reconcile"
    );
    assert!(
        recovered.compensation_plan.is_some(),
        "plan survived the crash"
    );
    assert!(
        recovered.pre_state.is_some(),
        "pre_state survived the crash"
    );
}

#[tokio::test]
async fn purge_removes_expired_row() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    // Fresh row (30d retention) — must survive.
    let fresh = journal::insert(&db, new_action(&sid, "op-fresh"))
        .await
        .unwrap();

    // Already-expired row (negative retention → retention_expires_at in the past).
    let mut expired_action = new_action(&sid, "op-expired");
    expired_action.retention_days = -1;
    let expired = journal::insert(&db, expired_action).await.unwrap();

    let removed = journal::purge_expired(&db).await.unwrap();
    assert_eq!(removed, 1, "exactly the expired row must be purged");
    assert!(journal::get_by_id(&db, &expired.id)
        .await
        .unwrap()
        .is_none());
    assert!(journal::get_by_id(&db, &fresh.id).await.unwrap().is_some());
}

/// Migration 0016: `turn_id` groups every journal row from one agent turn so
/// `list_by_turn` can collect them for the group-undo path (Harness Completion phase 2).
#[tokio::test]
async fn list_by_turn_collects_only_rows_sharing_the_turn_id() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let mut turn_a_1 = new_action(&sid, "turn-a-1");
    turn_a_1.turn_id = Some("turn-A");
    let a1 = journal::insert(&db, turn_a_1).await.unwrap();

    let mut turn_a_2 = new_action(&sid, "turn-a-2");
    turn_a_2.turn_id = Some("turn-A");
    let a2 = journal::insert(&db, turn_a_2).await.unwrap();

    let mut turn_b_1 = new_action(&sid, "turn-b-1");
    turn_b_1.turn_id = Some("turn-B");
    journal::insert(&db, turn_b_1).await.unwrap();

    // A row minted before this migration (or by a caller that never threaded turn_id).
    journal::insert(&db, new_action(&sid, "no-turn")).await.unwrap();

    let group = journal::list_by_turn(&db, "turn-A", &sid).await.unwrap();
    let ids: Vec<&str> = group.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(group.len(), 2, "only turn-A's two rows must be collected");
    assert!(ids.contains(&a1.id.as_str()));
    assert!(ids.contains(&a2.id.as_str()));
}

/// M1: a `turn_id` that exists but under a DIFFERENT session must yield nothing — the
/// same session-scoping boundary `get_by_id_scoped` already enforces for single-id lookup.
#[tokio::test]
async fn list_by_turn_cross_session_yields_nothing() {
    let (db, _dir) = setup().await;
    let owner_session = make_session(&db).await;
    let other_session = uuid::Uuid::new_v4().to_string();

    let mut action = new_action(&owner_session, "cross-sess-1");
    action.turn_id = Some("shared-turn-id");
    journal::insert(&db, action).await.unwrap();

    let as_owner = journal::list_by_turn(&db, "shared-turn-id", &owner_session)
        .await
        .unwrap();
    assert_eq!(as_owner.len(), 1, "the owning session sees its row");

    let as_other = journal::list_by_turn(&db, "shared-turn-id", &other_session)
        .await
        .unwrap();
    assert!(
        as_other.is_empty(),
        "a foreign session must never see another session's turn group"
    );
}
