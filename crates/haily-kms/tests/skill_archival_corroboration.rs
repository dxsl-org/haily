/// Harness Completion phase 5 — M1 review fix: decay-triggered archival must not
/// fire on the SAME weak signal that produced the low confidence in the first place.
/// A skill crossing below the archive threshold requires ≥2 independent negative-
/// labeled traces matched to it (Jaccard against its description) before
/// `apply_skill_decay` will archive it; otherwise it is held at its decayed
/// (low) confidence, still active.
use haily_db::{
    queries::skills::{self as db_skills, TraceMetrics},
    DbHandle,
};
use haily_kms::skills::apply_skill_decay;

async fn setup() -> (DbHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = DbHandle::init(&db_path).await.unwrap();
    (db, dir)
}

/// Fetch a skill row REGARDLESS of archived/deleted state — `db_skills::get_skill`
/// deliberately excludes archived rows (it exists for "targeted EMA updates" on
/// active skills only), so these tests use `get_skill_any_state` to observe the
/// archival outcome itself.
async fn get_skill_any_state(db: &DbHandle, id: &str) -> db_skills::Skill {
    db_skills::get_skill_any_state(db, id)
        .await
        .expect("query must succeed")
        .expect("skill row must exist")
}

/// Seed a skill whose confidence, AFTER one decay application, lands just below the
/// 0.30 archive threshold. `factor = exp(-0.693/24) ≈ 0.9716`; seeding at 0.305
/// decays to ≈0.2963 — comfortably below 0.30 but by a small enough margin that a
/// test failure due to floating-point drift is not a concern (ROUND(.., 4) in the
/// decay SQL rounds to 4 decimal places).
async fn seed_skill_just_above_threshold(db: &DbHandle, name: &str, description: &str) -> db_skills::Skill {
    let skill = db_skills::insert_skill(db, name, description, "pattern", "[]")
        .await
        .unwrap();
    db_skills::update_skill_confidence(db, &skill.id, 0.305, 1.0)
        .await
        .unwrap();
    db_skills::get_skill(db, &skill.id).await.unwrap().unwrap()
}

async fn insert_negative_trace(db: &DbHandle, session_id: &str, task_description: &str, label_source: &str) {
    db_skills::insert_trace(
        db,
        session_id,
        task_description,
        "[]",
        "failure",
        Some(100),
        TraceMetrics {
            label_source: Some(label_source),
            label_confidence: Some(0.6),
            ..TraceMetrics::default()
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn uncorroborated_low_confidence_skill_is_held_not_archived() {
    let (db, _dir) = setup().await;
    let skill = seed_skill_just_above_threshold(
        &db,
        "flaky-skill",
        "book a flight ticket to hanoi for the user",
    )
    .await;
    assert!(skill.confidence > 0.30, "sanity: seeded above threshold pre-decay");

    // Zero corroborating negative traces exist anywhere.
    apply_skill_decay(&db).await.expect("apply_skill_decay");

    let after = get_skill_any_state(&db, &skill.id).await;
    assert!(
        after.confidence < 0.30,
        "sanity: decay must still have pushed confidence below threshold, got {}",
        after.confidence
    );
    assert!(
        after.archived_at.is_none(),
        "an uncorroborated confidence collapse must NOT archive the skill — held at the floor instead"
    );
}

#[tokio::test]
async fn two_independent_negative_traces_corroborate_archival() {
    let (db, _dir) = setup().await;
    let skill = seed_skill_just_above_threshold(
        &db,
        "flaky-skill-2",
        "book a flight ticket to hanoi for the user",
    )
    .await;

    // Two independent negative traces, matched by Jaccard to the skill's description,
    // with DISTINCT label_source values (the stronger corroboration bar).
    let session_a = uuid::Uuid::new_v4().to_string();
    let session_b = uuid::Uuid::new_v4().to_string();
    insert_negative_trace(&db, &session_a, "book a flight ticket to hanoi", "tool_error_ratio").await;
    insert_negative_trace(&db, &session_b, "book a flight ticket to hanoi", "explicit_feedback").await;

    apply_skill_decay(&db).await.expect("apply_skill_decay");

    let after = get_skill_any_state(&db, &skill.id).await;
    assert!(
        after.archived_at.is_some(),
        "two independent, corroborating negative traces must allow archival"
    );
}

#[tokio::test]
async fn two_negatives_sharing_one_label_source_still_corroborate_the_at_minimum_floor() {
    let (db, _dir) = setup().await;
    let skill = seed_skill_just_above_threshold(
        &db,
        "flaky-skill-3",
        "book a flight ticket to hanoi for the user",
    )
    .await;

    // Two negative traces on DIFFERENT turns sharing the SAME label_source — the "at
    // minimum 2 independent negative-labeled traces" fallback in the phase's Risk
    // Notes still applies (2 distinct turns failing the same way is still
    // independent evidence, not a single restated signal).
    let session_a = uuid::Uuid::new_v4().to_string();
    let session_b = uuid::Uuid::new_v4().to_string();
    insert_negative_trace(&db, &session_a, "book a flight ticket to hanoi", "tool_error_ratio").await;
    insert_negative_trace(&db, &session_b, "book a flight ticket to hanoi", "tool_error_ratio").await;

    apply_skill_decay(&db).await.expect("apply_skill_decay");

    let after = get_skill_any_state(&db, &skill.id).await;
    assert!(
        after.archived_at.is_some(),
        "2 negative traces on different turns must clear the 'at minimum' corroboration floor even sharing one label_source"
    );
}

#[tokio::test]
async fn a_single_negative_trace_is_not_enough_to_corroborate() {
    let (db, _dir) = setup().await;
    let skill = seed_skill_just_above_threshold(
        &db,
        "flaky-skill-4",
        "book a flight ticket to hanoi for the user",
    )
    .await;

    let session_a = uuid::Uuid::new_v4().to_string();
    insert_negative_trace(&db, &session_a, "book a flight ticket to hanoi", "tool_error_ratio").await;

    apply_skill_decay(&db).await.expect("apply_skill_decay");

    let after = get_skill_any_state(&db, &skill.id).await;
    assert!(
        after.archived_at.is_none(),
        "MIN_CORROBORATING_NEGATIVES=2 — a single negative trace must not be enough to archive"
    );
}

#[tokio::test]
async fn negative_traces_for_an_unrelated_skill_do_not_corroborate() {
    let (db, _dir) = setup().await;
    let skill = seed_skill_just_above_threshold(
        &db,
        "flaky-skill-5",
        "book a flight ticket to hanoi for the user",
    )
    .await;

    // Negative traces exist, but their task_description has nothing to do with this
    // skill's description — must not match via Jaccard, so no corroboration.
    let session_a = uuid::Uuid::new_v4().to_string();
    let session_b = uuid::Uuid::new_v4().to_string();
    insert_negative_trace(&db, &session_a, "what is the capital of france", "tool_error_ratio").await;
    insert_negative_trace(&db, &session_b, "check the weather forecast today", "explicit_feedback").await;

    apply_skill_decay(&db).await.expect("apply_skill_decay");

    let after = get_skill_any_state(&db, &skill.id).await;
    assert!(
        after.archived_at.is_none(),
        "negative traces unrelated to this skill's description must not corroborate its archival"
    );
}

/// A skill whose confidence is still ABOVE the archive threshold after decay must
/// never be archived regardless of corroboration — the corroboration floor only
/// gates archival for candidates that ALREADY crossed the threshold.
#[tokio::test]
async fn a_skill_still_above_threshold_is_never_archived_even_with_corroboration() {
    let (db, _dir) = setup().await;
    let skill = db_skills::insert_skill(&db, "healthy-skill", "book a flight ticket to hanoi", "pattern", "[]")
        .await
        .unwrap();
    // Left at the default confidence (1.0) — nowhere near the 0.30 threshold even
    // after one decay application.

    let session_a = uuid::Uuid::new_v4().to_string();
    let session_b = uuid::Uuid::new_v4().to_string();
    insert_negative_trace(&db, &session_a, "book a flight ticket to hanoi", "tool_error_ratio").await;
    insert_negative_trace(&db, &session_b, "book a flight ticket to hanoi", "explicit_feedback").await;

    apply_skill_decay(&db).await.expect("apply_skill_decay");

    let after = get_skill_any_state(&db, &skill.id).await;
    assert!(
        after.archived_at.is_none(),
        "a skill whose confidence stays well above the threshold must not be archived"
    );
}
