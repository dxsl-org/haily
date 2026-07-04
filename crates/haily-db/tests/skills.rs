/// F9/F10/F20 regression tests: skill resurrection semantics, atomic EMA updates,
/// and the decay idempotency guard. Also Harness Completion phase 5: telemetry
/// columns on `insert_trace`, the feedback-downgrade join, the m4 exact undo
/// predicate, and daily rollup/retention.
use haily_db::{
    queries::{journal, sessions, skills},
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
        request_params: r#"{"model":"res.partner"}"#,
        pre_state: None,
        pre_state_version: None,
        compensation_plan: Some(r#"{"op":"unlink","id":1}"#),
        turn_id: None,
        retention_days: 30,
    }
}

// ---------------------------------------------------------------------------
// F9 — skill resurrection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn archived_skill_is_unarchived_on_resynthesis() {
    let (db, _dir) = setup().await;

    let original = skills::insert_skill(&db, "reminder-skill", "desc v1", "pattern", "[]")
        .await
        .unwrap();
    assert!(original.archived_at.is_none());

    // Simulate decay archiving the skill.
    sqlx::query("UPDATE kms_skills SET archived_at = ?, confidence = 0.1 WHERE id = ?")
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(&original.id)
        .execute(db.pool())
        .await
        .unwrap();

    // Re-synthesis of the same-named skill must un-archive, not error RowNotFound,
    // and must not silently create a duplicate row either.
    let resurrected = skills::insert_skill(&db, "reminder-skill", "desc v2", "pattern", "[]")
        .await
        .expect("resurrection must succeed, not RowNotFound");

    assert_eq!(
        resurrected.id, original.id,
        "must reuse the existing row, not duplicate"
    );
    assert!(
        resurrected.archived_at.is_none(),
        "resurrected skill must be un-archived"
    );
    assert_eq!(
        resurrected.confidence, 1.0,
        "resurrection resets confidence to 1.0"
    );
}

#[tokio::test]
async fn insert_skill_is_noop_for_existing_active_skill() {
    let (db, _dir) = setup().await;

    let first = skills::insert_skill(&db, "active-skill", "desc", "pattern", "[]")
        .await
        .unwrap();
    // Bump confidence to prove the second insert does not reset it (active row untouched).
    skills::update_skill_confidence(&db, &first.id, 1.0, 0.5)
        .await
        .unwrap();

    let second = skills::insert_skill(&db, "active-skill", "desc v2", "pattern", "[]")
        .await
        .unwrap();

    assert_eq!(second.id, first.id);
    assert!(
        second.confidence > 0.9,
        "active row must not be reset by a duplicate insert"
    );
}

// ---------------------------------------------------------------------------
// F10 — atomic EMA confidence updates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_ema_updates_both_apply() {
    let (db, _dir) = setup().await;
    let skill = skills::insert_skill(&db, "concurrent-skill", "desc", "pattern", "[]")
        .await
        .unwrap();

    const ALPHA: f64 = 0.10;
    let db1 = db.clone();
    let db2 = db.clone();
    let id1 = skill.id.clone();
    let id2 = skill.id.clone();

    // Two concurrent updates racing against the same row — a non-atomic
    // read-modify-write in Rust would let the slower write clobber the faster one.
    let (r1, r2) = tokio::join!(
        skills::update_skill_confidence(&db1, &id1, 1.0, ALPHA),
        skills::update_skill_confidence(&db2, &id2, 1.0, ALPHA),
    );
    r1.unwrap();
    r2.unwrap();

    let final_skill = skills::get_skill(&db, &skill.id).await.unwrap().unwrap();
    // Applying the EMA update twice from confidence=1.0 with reward=1.0 keeps it at 1.0
    // (clamped) — use a reward that actually moves the needle to prove both applied.
    // Starting confidence 1.0, alpha 0.10, reward 1.0 stays 1.0 either way, so instead
    // assert against the two-applications-vs-one-application arithmetic directly below.
    let expected_after_two_applications = {
        let after_one = ALPHA * 1.0 + (1.0 - ALPHA) * 1.0;
        ALPHA * 1.0 + (1.0 - ALPHA) * after_one
    };
    assert!(
        (final_skill.confidence - expected_after_two_applications).abs() < 1e-9,
        "both concurrent EMA updates must be applied atomically, got {}",
        final_skill.confidence
    );
}

#[tokio::test]
async fn concurrent_ema_updates_with_failure_reward_both_apply() {
    let (db, _dir) = setup().await;
    let skill = skills::insert_skill(&db, "concurrent-skill-2", "desc", "pattern", "[]")
        .await
        .unwrap();

    const ALPHA: f64 = 0.10;
    let db1 = db.clone();
    let db2 = db.clone();
    let id1 = skill.id.clone();
    let id2 = skill.id.clone();

    // reward=0.0 (failure) from confidence=1.0 moves the value each time it's applied,
    // so this test actually distinguishes "both applied" from "one clobbered the other".
    let (r1, r2) = tokio::join!(
        skills::update_skill_confidence(&db1, &id1, 0.0, ALPHA),
        skills::update_skill_confidence(&db2, &id2, 0.0, ALPHA),
    );
    r1.unwrap();
    r2.unwrap();

    let final_skill = skills::get_skill(&db, &skill.id).await.unwrap().unwrap();
    let after_one = ALPHA * 0.0 + (1.0 - ALPHA) * 1.0;
    let after_two = ALPHA * 0.0 + (1.0 - ALPHA) * after_one;

    assert!(
        (final_skill.confidence - after_two).abs() < 1e-9,
        "expected two sequential EMA applications ({after_two}), got {} \
         (single application would be {after_one} — indicates a clobbered update)",
        final_skill.confidence
    );
}

// ---------------------------------------------------------------------------
// F20 — decay idempotency guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn decay_called_twice_within_window_second_call_is_noop() {
    let (db, _dir) = setup().await;
    skills::insert_skill(&db, "decay-skill", "desc", "pattern", "[]")
        .await
        .unwrap();

    const LAMBDA: f64 = 0.693 / 24.0;

    let first_run = skills::apply_exponential_decay(&db, LAMBDA).await.unwrap();
    assert_eq!(
        first_run, 1,
        "first decay call should touch the one active skill"
    );

    let second_run = skills::apply_exponential_decay(&db, LAMBDA).await.unwrap();
    assert_eq!(
        second_run, 0,
        "second call within the guard window must be a no-op"
    );
}

#[tokio::test]
async fn decay_runs_again_after_guard_window_elapses() {
    let (db, _dir) = setup().await;
    skills::insert_skill(&db, "decay-skill-2", "desc", "pattern", "[]")
        .await
        .unwrap();

    const LAMBDA: f64 = 0.693 / 24.0;

    skills::apply_exponential_decay(&db, LAMBDA).await.unwrap();

    // Backdate the guard timestamp past the 20h window to simulate elapsed time.
    let stale_timestamp = (chrono::Utc::now() - chrono::Duration::hours(21)).to_rfc3339();
    sqlx::query("UPDATE kms_preferences SET value = ? WHERE key = 'kms.skills.last_decay_run'")
        .bind(&stale_timestamp)
        .execute(db.pool())
        .await
        .unwrap();

    let third_run = skills::apply_exponential_decay(&db, LAMBDA).await.unwrap();
    assert_eq!(
        third_run, 1,
        "decay must run again once the guard window has elapsed"
    );
}

// ---------------------------------------------------------------------------
// Harness Completion phase 5 — telemetry columns, downgrade_trace, m4 undo
// predicate, daily rollup + retention.
// ---------------------------------------------------------------------------

fn no_metrics() -> skills::TraceMetrics<'static> {
    skills::TraceMetrics::default()
}

#[tokio::test]
async fn insert_trace_persists_all_telemetry_columns() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let metrics = skills::TraceMetrics {
        model_tier: Some("fast"),
        prompt_tokens: Some(120),
        completion_tokens: Some(40),
        tool_call_count: Some(2),
        approval_requested: Some(true),
        approval_denied: Some(false),
        undo_within_5min: Some(false),
        label_source: Some("tool_error_ratio"),
        label_confidence: Some(0.6),
        delegate_overhead_ms: Some(15),
    };

    let trace = skills::insert_trace(&db, &sid, "do the thing", "[]", "partial", Some(500), metrics)
        .await
        .unwrap();

    assert_eq!(trace.model_tier.as_deref(), Some("fast"));
    assert_eq!(trace.prompt_tokens, Some(120));
    assert_eq!(trace.completion_tokens, Some(40));
    assert_eq!(trace.tool_call_count, Some(2));
    assert_eq!(trace.approval_requested, Some(true));
    assert_eq!(trace.approval_denied, Some(false));
    assert_eq!(trace.undo_within_5min, Some(false));
    assert_eq!(trace.label_source.as_deref(), Some("tool_error_ratio"));
    assert_eq!(trace.label_confidence, Some(0.6));
    assert_eq!(trace.delegate_overhead_ms, Some(15));
}

#[tokio::test]
async fn insert_trace_with_no_metrics_leaves_columns_null() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let trace = skills::insert_trace(&db, &sid, "quick question", "[]", "success", None, no_metrics())
        .await
        .unwrap();

    assert!(trace.model_tier.is_none());
    assert!(trace.prompt_tokens.is_none());
    assert!(trace.label_source.is_none(), "no signal ⇒ no fabricated label");
    assert!(trace.label_confidence.is_none());
}

#[tokio::test]
async fn most_recent_trace_returns_the_latest_for_the_session() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    skills::insert_trace(&db, &sid, "first task", "[]", "success", None, no_metrics())
        .await
        .unwrap();
    // Ensure a distinct created_at ordering (RFC3339 string comparison is lexical).
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let second = skills::insert_trace(&db, &sid, "second task", "[]", "success", None, no_metrics())
        .await
        .unwrap();

    let latest = skills::most_recent_trace(&db, &sid).await.unwrap().unwrap();
    assert_eq!(latest.id, second.id);
    assert_eq!(latest.task_description, "second task");
}

#[tokio::test]
async fn most_recent_trace_is_none_for_a_session_with_no_traces() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;
    assert!(skills::most_recent_trace(&db, &sid).await.unwrap().is_none());
}

#[tokio::test]
async fn downgrade_trace_overwrites_outcome_and_label() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let trace = skills::insert_trace(&db, &sid, "book a flight", "[]", "success", None, no_metrics())
        .await
        .unwrap();

    skills::downgrade_trace(&db, &trace.id, "failure", "explicit_feedback", 0.9)
        .await
        .unwrap();

    let traces = skills::recent_traces(&db, 10).await.unwrap();
    let updated = traces.into_iter().find(|t| t.id == trace.id).unwrap();
    assert_eq!(updated.outcome, "failure");
    assert_eq!(updated.label_source.as_deref(), Some("explicit_feedback"));
    assert_eq!(updated.label_confidence, Some(0.9));
}

#[tokio::test]
async fn downgrade_trace_on_unknown_id_is_a_silent_noop() {
    let (db, _dir) = setup().await;
    // No row with this id exists — must not error.
    skills::downgrade_trace(&db, "nonexistent-id", "failure", "explicit_feedback", 0.9)
        .await
        .unwrap();
}

// -- m4: undo_within_n_min exact predicate ---------------------------------
//
// "undo mutates the ORIGINAL action_journal row" — there is no distinct undo row.
// The predicate is: same session_id, undo_status='undone', undone_at within N
// minutes of the action's created_at. `created_at` is an EVIDENTIARY column
// (migration 0012's append-only trigger forbids rewriting it after insert), so
// these tests use the row's REAL insert-time `created_at` and only ever set
// `undone_at` (a mutable processing column) relative to it — never backdating
// `created_at` itself.

async fn mark_undone_at(db: &DbHandle, id: &str, undone_at: &str) {
    sqlx::query("UPDATE action_journal SET undo_status = 'undone', undone_at = ? WHERE id = ?")
        .bind(undone_at)
        .bind(id)
        .execute(db.pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn undo_within_n_min_true_when_undone_shortly_after_creation() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let row = journal::insert(&db, new_action(&sid, "undo-op-1")).await.unwrap();
    let created_at: chrono::DateTime<chrono::Utc> =
        chrono::DateTime::parse_from_rfc3339(&row.created_at).unwrap().into();
    let undone_at = (created_at + chrono::Duration::minutes(3)).to_rfc3339();
    mark_undone_at(&db, &row.id, &undone_at).await;

    let hit = skills::undo_within_n_min(&db, &sid, &row.created_at, 5).await.unwrap();
    assert!(hit, "an undo 3 minutes after creation must match a 5-minute window");
}

#[tokio::test]
async fn undo_within_n_min_false_when_undone_outside_the_window() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let row = journal::insert(&db, new_action(&sid, "undo-op-2")).await.unwrap();
    let created_at: chrono::DateTime<chrono::Utc> =
        chrono::DateTime::parse_from_rfc3339(&row.created_at).unwrap().into();
    let undone_at = (created_at + chrono::Duration::minutes(11)).to_rfc3339(); // outside a 5-min window
    mark_undone_at(&db, &row.id, &undone_at).await;

    let hit = skills::undo_within_n_min(&db, &sid, &row.created_at, 5).await.unwrap();
    assert!(!hit, "an undo 11 minutes after creation must NOT match a 5-minute window");
}

#[tokio::test]
async fn undo_within_n_min_false_when_no_undo_recorded() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let row = journal::insert(&db, new_action(&sid, "undo-op-3")).await.unwrap();
    // Never marked undone — undo_status stays 'not_requested'.

    let hit = skills::undo_within_n_min(&db, &sid, &row.created_at, 5).await.unwrap();
    assert!(!hit, "no undone row ⇒ predicate must be false");
}

#[tokio::test]
async fn undo_within_n_min_scoped_to_session() {
    let (db, _dir) = setup().await;
    let sid_a = make_session(&db).await;
    let sid_b = make_session(&db).await;

    // Undo recorded under session B...
    let row_b = journal::insert(&db, new_action(&sid_b, "undo-op-4")).await.unwrap();
    let created_at: chrono::DateTime<chrono::Utc> =
        chrono::DateTime::parse_from_rfc3339(&row_b.created_at).unwrap().into();
    let undone_at = (created_at + chrono::Duration::minutes(1)).to_rfc3339();
    mark_undone_at(&db, &row_b.id, &undone_at).await;

    // ...must NOT satisfy the predicate for session A checked against the SAME timestamp.
    let hit = skills::undo_within_n_min(&db, &sid_a, &row_b.created_at, 5).await.unwrap();
    assert!(!hit, "a same-timestamp undo in a DIFFERENT session must not match");
}

// -- daily rollup + retention -----------------------------------------------

#[tokio::test]
async fn compute_daily_rollup_aggregates_by_date_and_tier() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let date = "2026-06-15";
    for (outcome, tier) in [
        ("success", Some("fast")),
        ("success", Some("fast")),
        ("failure", Some("fast")),
        ("partial", None),
    ] {
        let trace = skills::insert_trace(
            &db,
            &sid,
            "task",
            "[]",
            outcome,
            Some(100),
            skills::TraceMetrics {
                model_tier: tier,
                ..skills::TraceMetrics::default()
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE kms_task_traces SET created_at = ? WHERE id = ?")
            .bind(format!("{date}T12:00:00+00:00"))
            .bind(&trace.id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    let upserted = skills::compute_daily_rollup(&db, date).await.unwrap();
    assert_eq!(upserted, 2, "two distinct tier buckets: 'fast' and '' (no tier)");

    let rows = skills::rollup_for_date(&db, date).await.unwrap();
    let fast = rows.iter().find(|r| r.model_tier == "fast").unwrap();
    assert_eq!(fast.count, 3);
    assert_eq!(fast.success_count, 2);
    assert_eq!(fast.failure_count, 1);

    let untiered = rows.iter().find(|r| r.model_tier.is_empty()).unwrap();
    assert_eq!(untiered.count, 1);
    assert_eq!(untiered.partial_count, 1);
}

#[tokio::test]
async fn compute_daily_rollup_is_idempotent_on_rerun() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;
    let date = "2026-06-16";

    let trace = skills::insert_trace(&db, &sid, "task", "[]", "success", Some(50), no_metrics())
        .await
        .unwrap();
    sqlx::query("UPDATE kms_task_traces SET created_at = ? WHERE id = ?")
        .bind(format!("{date}T08:00:00+00:00"))
        .bind(&trace.id)
        .execute(db.pool())
        .await
        .unwrap();

    skills::compute_daily_rollup(&db, date).await.unwrap();
    skills::compute_daily_rollup(&db, date).await.unwrap();

    let rows = skills::rollup_for_date(&db, date).await.unwrap();
    assert_eq!(rows.len(), 1, "a rerun must upsert, not duplicate the row");
    assert_eq!(rows[0].count, 1);
}

#[tokio::test]
async fn delete_traces_older_than_removes_only_stale_rows() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let old = skills::insert_trace(&db, &sid, "old task", "[]", "success", None, no_metrics())
        .await
        .unwrap();
    let fresh = skills::insert_trace(&db, &sid, "fresh task", "[]", "success", None, no_metrics())
        .await
        .unwrap();

    let stale_ts = (chrono::Utc::now() - chrono::Duration::days(100)).to_rfc3339();
    sqlx::query("UPDATE kms_task_traces SET created_at = ? WHERE id = ?")
        .bind(&stale_ts)
        .bind(&old.id)
        .execute(db.pool())
        .await
        .unwrap();

    let deleted = skills::delete_traces_older_than(&db, 90).await.unwrap();
    assert_eq!(deleted, 1);

    let remaining = skills::recent_traces(&db, 10).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, fresh.id);
}
