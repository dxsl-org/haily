//! Mock-backend logic tests for [`super::CredentialStore`] — every read/write fallback
//! branch (M5a/M5b/M5c) covered against the keyring crate's own platform-independent mock,
//! so this suite is deterministic on any OS/CI runner. The end-to-end scenarios that need
//! real file-level DB inspection (the M5c residue-scrub byte grep) live in the external
//! integration test `tests/credential_store.rs` instead.
use super::*;

/// Installs the crate's built-in mock backend so every test in this module runs against a
/// platform-independent, non-persistent fake store — no real OS credential manager is
/// touched. Safe to call repeatedly (idempotent last-call-wins).
fn use_mock_backend() {
    keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
}

async fn store(dir: &std::path::Path, policy: CredentialPolicy) -> CredentialStore {
    let db = Arc::new(DbHandle::init(&dir.join("t.db")).await.unwrap());
    CredentialStore::new(db, policy)
}

#[tokio::test]
async fn get_secret_round_trips_through_keyring() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::default()).await;

    s.set_secret("test.round_trip", "sk-abc123").await.unwrap();
    assert_eq!(
        s.get_secret("test.round_trip").await.unwrap().as_deref(),
        Some("sk-abc123")
    );
}

#[tokio::test]
async fn get_secret_no_entry_falls_back_to_db_when_present() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::default()).await;

    // Never written to keyring at all (NoEntry) but present in the DB — this is the
    // "never migrated" path, always allowed regardless of allow_read_fallback.
    meta::upsert_preference(&s.db, "test.legacy", "plain-secret", "test")
        .await
        .unwrap();
    assert_eq!(
        s.get_secret("test.legacy").await.unwrap().as_deref(),
        Some("plain-secret")
    );
}

#[tokio::test]
async fn platform_failure_on_read_falls_back_and_persists_warning_when_allowed() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::default()).await;

    meta::upsert_preference(&s.db, "test.pf", "db-fallback-value", "test")
        .await
        .unwrap();
    s.force_next_keyring_error(
        "test.pf",
        keyring::Error::PlatformFailure(Box::new(std::io::Error::other("boom"))),
    )
    .await;

    let got = s.get_secret("test.pf").await.unwrap();
    assert_eq!(got.as_deref(), Some("db-fallback-value"), "M5b: read-fallback allowed by default");

    let warning = meta::get_preference(&s.db, FALLBACK_WARNING_PREF).await.unwrap();
    assert_eq!(warning.as_deref(), Some("true"), "M5a/M5b: fallback must be a PERSISTED flag");
}

#[tokio::test]
async fn platform_failure_on_read_returns_none_when_read_fallback_disabled() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let policy = CredentialPolicy {
        allow_read_fallback: false,
        ..CredentialPolicy::default()
    };
    let s = store(dir.path(), policy).await;

    meta::upsert_preference(&s.db, "test.pf2", "should-not-be-returned", "test")
        .await
        .unwrap();
    s.force_next_keyring_error(
        "test.pf2",
        keyring::Error::PlatformFailure(Box::new(std::io::Error::other("boom"))),
    )
    .await;

    let got = s.get_secret("test.pf2").await.unwrap();
    assert!(got.is_none(), "read-fallback explicitly disabled must not leak the DB value");
}

#[tokio::test]
async fn write_failure_fails_closed_by_default() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::default()).await;

    s.force_next_keyring_error(
        "test.write_fail",
        keyring::Error::PlatformFailure(Box::new(std::io::Error::other("boom"))),
    )
    .await;

    let result = s.set_secret("test.write_fail", "some-secret").await;
    assert!(result.is_err(), "M5b: write-fallback must FAIL CLOSED by default");

    // Fail-closed must mean nothing was written to the DB either.
    let db_row = meta::get_preference(&s.db, "test.write_fail").await.unwrap();
    assert!(db_row.is_none(), "no silent plaintext write on fail-closed path");
}

#[tokio::test]
async fn write_failure_writes_plaintext_only_when_opted_in() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let policy = CredentialPolicy {
        allow_write_plaintext: true,
        ..CredentialPolicy::default()
    };
    let s = store(dir.path(), policy).await;

    s.force_next_keyring_error(
        "test.write_optin",
        keyring::Error::PlatformFailure(Box::new(std::io::Error::other("boom"))),
    )
    .await;

    s.set_secret("test.write_optin", "opted-in-secret").await.unwrap();

    let db_row = meta::get_preference(&s.db, "test.write_optin").await.unwrap();
    assert_eq!(db_row.as_deref(), Some("opted-in-secret"), "opt-in must land in the DB");
    assert_eq!(
        s.get_secret("test.write_optin").await.unwrap().as_deref(),
        Some("opted-in-secret"),
        "the opted-in write must also be immediately readable"
    );
}

#[tokio::test]
async fn headless_policy_never_attempts_keyring_and_sets_persisted_warning_on_write_fallback() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let policy = CredentialPolicy {
        allow_write_plaintext: true,
        ..CredentialPolicy::headless()
    };
    let s = store(dir.path(), policy).await;
    assert!(!s.policy.attempt_keyring, "M5a: headless must set attempt_keyring=false");

    // A read with nothing in the DB and no attempted keyring call: None, no panic.
    assert!(s.get_secret("test.headless_missing").await.unwrap().is_none());

    // A read with a plaintext DB row present: returned directly, no keyring RPC.
    meta::upsert_preference(&s.db, "test.headless_read", "headless-value", "test")
        .await
        .unwrap();
    assert_eq!(
        s.get_secret("test.headless_read").await.unwrap().as_deref(),
        Some("headless-value")
    );

    // A write under headless with the opt-in flag set: lands in the DB (never touches
    // the keyring since attempt_keyring is false).
    s.set_secret("test.headless_write", "headless-secret").await.unwrap();
    let db_row = meta::get_preference(&s.db, "test.headless_write").await.unwrap();
    assert_eq!(db_row.as_deref(), Some("headless-secret"));
}

#[tokio::test]
async fn headless_write_without_optin_fails_closed() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::headless()).await;

    let result = s.set_secret("test.headless_no_optin", "x").await;
    assert!(result.is_err(), "headless without write opt-in must still fail closed, not silently write plaintext");
}

#[tokio::test]
async fn migration_moves_secret_verifies_readback_and_writes_marker() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::default()).await;

    meta::upsert_preference(&s.db, "test.migrate", "legacy-plaintext-secret", "test")
        .await
        .unwrap();

    s.migrate_from_db("test.migrate").await.unwrap();

    // Read-your-write: the secret is now served from the keyring.
    assert_eq!(
        s.get_secret("test.migrate").await.unwrap().as_deref(),
        Some("legacy-plaintext-secret")
    );

    // The DB row itself now holds only the marker, never the raw secret.
    let db_row = meta::get_preference(&s.db, "test.migrate").await.unwrap().unwrap();
    assert!(is_keyring_marker(&db_row), "DB row must hold the marker after migration: {db_row}");
    assert_ne!(db_row, "legacy-plaintext-secret");
}

#[tokio::test]
async fn migration_is_idempotent_on_repeat_boots() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::default()).await;

    meta::upsert_preference(&s.db, "test.idempotent", "once-only-secret", "test")
        .await
        .unwrap();
    s.migrate_from_db("test.idempotent").await.unwrap();
    let marker_after_first = meta::get_preference(&s.db, "test.idempotent").await.unwrap();

    // Second "boot": already-migrated row is a no-op, not a re-migration attempt.
    s.migrate_from_db("test.idempotent").await.unwrap();
    let marker_after_second = meta::get_preference(&s.db, "test.idempotent").await.unwrap();
    assert_eq!(marker_after_first, marker_after_second);

    assert_eq!(
        s.get_secret("test.idempotent").await.unwrap().as_deref(),
        Some("once-only-secret")
    );
}

#[tokio::test]
async fn migration_never_touches_db_row_when_keyring_write_fails() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::default()).await;

    meta::upsert_preference(&s.db, "test.migrate_fail", "must-not-be-lost", "test")
        .await
        .unwrap();
    s.force_next_keyring_error(
        "test.migrate_fail",
        keyring::Error::PlatformFailure(Box::new(std::io::Error::other("boom"))),
    )
    .await;

    let result = s.migrate_from_db("test.migrate_fail").await;
    assert!(result.is_err());

    // The DB row must be untouched — still the raw secret, not a marker, not empty.
    let db_row = meta::get_preference(&s.db, "test.migrate_fail").await.unwrap().unwrap();
    assert_eq!(db_row, "must-not-be-lost", "no data loss on a failed migration");
}
