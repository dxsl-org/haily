/// Harness Completion phase 5, Gap B / m2 — `apply_feedback_signal` joins a
/// `Negative`/`Correction` signal to the session's prior trace and downgrades it,
/// with an explicit-vs-phrase confidence split. These tests exercise the REAL DB
/// path (not just the pure `derive_label` function) — `insert_trace` +
/// `apply_feedback_signal` + `recent_traces` round-tripped against a temp SQLite file.
use haily_db::{
    queries::skills as db_skills,
    DbHandle,
};
use haily_kms::feedback::{apply_feedback_signal, FeedbackSignal};
use haily_kms::skills::{EXPLICIT_FEEDBACK_CONFIDENCE, PHRASE_FEEDBACK_CONFIDENCE};

async fn setup() -> (DbHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = DbHandle::init(&db_path).await.unwrap();
    (db, dir)
}

async fn seed_trace(db: &DbHandle, session_id: &str, task: &str) -> db_skills::TaskTrace {
    db_skills::insert_trace(
        db,
        session_id,
        task,
        "[]",
        "success",
        Some(200),
        db_skills::TraceMetrics::default(),
    )
    .await
    .unwrap()
}

async fn trace_by_id(db: &DbHandle, id: &str) -> db_skills::TaskTrace {
    db_skills::recent_traces(db, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.id == id)
        .expect("trace must still exist")
}

#[tokio::test]
async fn explicit_negative_feedback_downgrades_the_prior_trace_to_failure() {
    let (db, _dir) = setup().await;
    let session_id = uuid::Uuid::new_v4().to_string();
    let trace = seed_trace(&db, &session_id, "book a flight").await;

    let signal = FeedbackSignal::Negative { topic: None };
    apply_feedback_signal(&signal, &db, &session_id, true)
        .await
        .unwrap();

    let updated = trace_by_id(&db, &trace.id).await;
    assert_eq!(updated.outcome, "failure");
    assert_eq!(updated.label_source.as_deref(), Some("explicit_feedback"));
    assert_eq!(updated.label_confidence, Some(EXPLICIT_FEEDBACK_CONFIDENCE));
}

#[tokio::test]
async fn phrase_detected_negative_feedback_downgrades_with_capped_confidence() {
    let (db, _dir) = setup().await;
    let session_id = uuid::Uuid::new_v4().to_string();
    let trace = seed_trace(&db, &session_id, "book a flight").await;

    let signal = FeedbackSignal::Negative { topic: None };
    apply_feedback_signal(&signal, &db, &session_id, false)
        .await
        .unwrap();

    let updated = trace_by_id(&db, &trace.id).await;
    assert_eq!(updated.outcome, "failure");
    assert_eq!(updated.label_source.as_deref(), Some("phrase_feedback"));
    assert_eq!(updated.label_confidence, Some(PHRASE_FEEDBACK_CONFIDENCE));
}

/// m2's confidence-cap property, proven end-to-end through the real downgrade path
/// (not just the constant comparison in `outcome_label.rs`): the SAME session/trace
/// downgraded via an explicit signal ends up with strictly higher label_confidence
/// than one downgraded via a phrase-detected signal.
#[tokio::test]
async fn explicit_downgrade_confidence_is_strictly_higher_than_phrase_downgrade() {
    let (db, _dir) = setup().await;

    let explicit_session = uuid::Uuid::new_v4().to_string();
    let explicit_trace = seed_trace(&db, &explicit_session, "book a flight").await;
    apply_feedback_signal(
        &FeedbackSignal::Correction { old: "hanoi".into(), new: "saigon".into() },
        &db,
        &explicit_session,
        true,
    )
    .await
    .unwrap();

    let phrase_session = uuid::Uuid::new_v4().to_string();
    let phrase_trace = seed_trace(&db, &phrase_session, "book a flight").await;
    apply_feedback_signal(
        &FeedbackSignal::Correction { old: "hanoi".into(), new: "saigon".into() },
        &db,
        &phrase_session,
        false,
    )
    .await
    .unwrap();

    let explicit_updated = trace_by_id(&db, &explicit_trace.id).await;
    let phrase_updated = trace_by_id(&db, &phrase_trace.id).await;

    let explicit_conf = explicit_updated.label_confidence.expect("explicit must set a confidence");
    let phrase_conf = phrase_updated.label_confidence.expect("phrase must set a confidence");
    assert!(
        explicit_conf > phrase_conf,
        "m2: explicit ({explicit_conf}) must exceed phrase-detected ({phrase_conf})"
    );
}

#[tokio::test]
async fn positive_feedback_does_not_touch_any_trace() {
    let (db, _dir) = setup().await;
    let session_id = uuid::Uuid::new_v4().to_string();
    let trace = seed_trace(&db, &session_id, "book a flight").await;

    apply_feedback_signal(&FeedbackSignal::Positive, &db, &session_id, true)
        .await
        .unwrap();

    let unchanged = trace_by_id(&db, &trace.id).await;
    assert_eq!(unchanged.outcome, "success", "Positive must never downgrade a trace");
    assert!(unchanged.label_source.is_none());
}

#[tokio::test]
async fn negative_feedback_with_no_prior_trace_is_a_harmless_noop() {
    let (db, _dir) = setup().await;
    let session_id = uuid::Uuid::new_v4().to_string();
    // No trace seeded for this session at all.
    let result = apply_feedback_signal(
        &FeedbackSignal::Negative { topic: None },
        &db,
        &session_id,
        true,
    )
    .await;
    assert!(result.is_ok(), "must not error when there is nothing to downgrade");
}

/// The downgrade must target the session's OWN most recent trace, not bleed across
/// sessions — a negative signal in session A must never touch session B's trace.
#[tokio::test]
async fn downgrade_is_scoped_to_the_signaling_session() {
    let (db, _dir) = setup().await;
    let session_a = uuid::Uuid::new_v4().to_string();
    let session_b = uuid::Uuid::new_v4().to_string();

    let trace_a = seed_trace(&db, &session_a, "task in session A").await;
    let trace_b = seed_trace(&db, &session_b, "task in session B").await;

    apply_feedback_signal(&FeedbackSignal::Negative { topic: None }, &db, &session_a, true)
        .await
        .unwrap();

    let updated_a = trace_by_id(&db, &trace_a.id).await;
    let updated_b = trace_by_id(&db, &trace_b.id).await;
    assert_eq!(updated_a.outcome, "failure");
    assert_eq!(updated_b.outcome, "success", "session B's trace must be untouched");
}
