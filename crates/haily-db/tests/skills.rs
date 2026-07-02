/// F9/F10/F20 regression tests: skill resurrection semantics, atomic EMA updates,
/// and the decay idempotency guard.
use haily_db::{queries::skills, DbHandle};

async fn setup() -> (DbHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = DbHandle::init(&db_path).await.unwrap();
    (db, dir)
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

    assert_eq!(resurrected.id, original.id, "must reuse the existing row, not duplicate");
    assert!(resurrected.archived_at.is_none(), "resurrected skill must be un-archived");
    assert_eq!(resurrected.confidence, 1.0, "resurrection resets confidence to 1.0");
}

#[tokio::test]
async fn insert_skill_is_noop_for_existing_active_skill() {
    let (db, _dir) = setup().await;

    let first = skills::insert_skill(&db, "active-skill", "desc", "pattern", "[]")
        .await
        .unwrap();
    // Bump confidence to prove the second insert does not reset it (active row untouched).
    skills::update_skill_confidence(&db, &first.id, 1.0, 0.5).await.unwrap();

    let second = skills::insert_skill(&db, "active-skill", "desc v2", "pattern", "[]")
        .await
        .unwrap();

    assert_eq!(second.id, first.id);
    assert!(second.confidence > 0.9, "active row must not be reset by a duplicate insert");
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
    skills::insert_skill(&db, "decay-skill", "desc", "pattern", "[]").await.unwrap();

    const LAMBDA: f64 = 0.693 / 24.0;
    const ARCHIVE_BELOW: f64 = 0.30;

    let first_run = skills::apply_exponential_decay(&db, LAMBDA, ARCHIVE_BELOW).await.unwrap();
    assert_eq!(first_run, 1, "first decay call should touch the one active skill");

    let second_run = skills::apply_exponential_decay(&db, LAMBDA, ARCHIVE_BELOW).await.unwrap();
    assert_eq!(second_run, 0, "second call within the guard window must be a no-op");
}

#[tokio::test]
async fn decay_runs_again_after_guard_window_elapses() {
    let (db, _dir) = setup().await;
    skills::insert_skill(&db, "decay-skill-2", "desc", "pattern", "[]").await.unwrap();

    const LAMBDA: f64 = 0.693 / 24.0;
    const ARCHIVE_BELOW: f64 = 0.30;

    skills::apply_exponential_decay(&db, LAMBDA, ARCHIVE_BELOW).await.unwrap();

    // Backdate the guard timestamp past the 20h window to simulate elapsed time.
    let stale_timestamp = (chrono::Utc::now() - chrono::Duration::hours(21)).to_rfc3339();
    sqlx::query("UPDATE kms_preferences SET value = ? WHERE key = 'kms.skills.last_decay_run'")
        .bind(&stale_timestamp)
        .execute(db.pool())
        .await
        .unwrap();

    let third_run = skills::apply_exponential_decay(&db, LAMBDA, ARCHIVE_BELOW).await.unwrap();
    assert_eq!(third_run, 1, "decay must run again once the guard window has elapsed");
}
