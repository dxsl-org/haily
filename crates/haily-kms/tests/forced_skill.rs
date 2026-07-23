//! Unified Chat UI phase 2 — `Request.forced_skill` wire-injection defense.
//!
//! `build_life_context`'s `forced_skill` param is re-validated against the live
//! `SkillGates` AT READ TIME (not just at slash-registry build time), because
//! `Request.forced_skill` is `#[serde(default)]`, not `#[serde(skip)]` like `origin` — a
//! crafted/replayed payload could otherwise name a disabled skill directly.
use haily_db::{
    queries::{meta, skills as db_skills},
    DbHandle,
};
use haily_kms::{skill_gates::SKILL_ENABLED_PREFIX, KmsHandle};

async fn setup() -> (DbHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = DbHandle::init(&db_path).await.unwrap();
    (db, dir)
}

/// Enabled synthesized skill named in `forced_skill` → its body reaches the assembled
/// `LifeContext` (and, transitively, the rendered system prompt).
#[tokio::test]
async fn enabled_synthesized_forced_skill_body_is_injected() {
    let (db, dir) = setup().await;
    db_skills::insert_skill(
        &db,
        "fix-bug",
        "diagnose and fix a bug",
        "pattern",
        r#"["reproduce","find root cause","apply fix"]"#,
    )
    .await
    .unwrap();

    let kms = KmsHandle::init(db, dir.path()).await.expect("kms init");
    let ctx = kms
        .build_life_context(uuid::Uuid::new_v4(), Some("fix-bug"))
        .await
        .expect("build_life_context");

    let forced = ctx.forced_skill.expect("enabled skill must be injected");
    assert_eq!(forced.name, "fix-bug");
    assert!(forced.body.contains("diagnose and fix a bug"));
    assert!(forced.body.contains("reproduce"));
}

/// A skill explicitly disabled via `SkillGates` (`skill.enabled.<name> = "false"`) must NOT
/// be injected even though it exists and is named exactly — the wire-injection defense.
#[tokio::test]
async fn disabled_forced_skill_is_not_injected() {
    let (db, dir) = setup().await;
    db_skills::insert_skill(&db, "fix-bug", "diagnose and fix a bug", "pattern", "[]")
        .await
        .unwrap();
    meta::upsert_preference(&db, &format!("{SKILL_ENABLED_PREFIX}fix-bug"), "false", "gui")
        .await
        .unwrap();

    let kms = KmsHandle::init(db, dir.path()).await.expect("kms init");
    let ctx = kms
        .build_life_context(uuid::Uuid::new_v4(), Some("fix-bug"))
        .await
        .expect("build_life_context");

    assert!(
        ctx.forced_skill.is_none(),
        "a gate-disabled forced_skill name must never be injected"
    );
}

/// A name that matches no authored or synthesized skill at all (e.g. an archived/deleted
/// row, which `active_skills` already excludes, or a name that never existed) yields `None`
/// rather than an error — a crafted `forced_skill` can never crash context assembly.
#[tokio::test]
async fn unknown_forced_skill_name_is_not_injected() {
    let (db, dir) = setup().await;

    let kms = KmsHandle::init(db, dir.path()).await.expect("kms init");
    let ctx = kms
        .build_life_context(uuid::Uuid::new_v4(), Some("does-not-exist"))
        .await
        .expect("build_life_context");

    assert!(ctx.forced_skill.is_none());
}

/// No `forced_skill` at all (the overwhelmingly common case — every normal turn) leaves the
/// field `None`, never attempting a lookup.
#[tokio::test]
async fn absent_forced_skill_yields_none() {
    let (db, dir) = setup().await;
    let kms = KmsHandle::init(db, dir.path()).await.expect("kms init");
    let ctx = kms
        .build_life_context(uuid::Uuid::new_v4(), None)
        .await
        .expect("build_life_context");
    assert!(ctx.forced_skill.is_none());
}
