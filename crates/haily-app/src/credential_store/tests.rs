//! Mock-backend logic tests for [`super::CredentialStore`] — every read/write fallback
//! branch (M5a/M5b/M5c) covered against the keyring crate's own platform-independent mock,
//! so this suite is deterministic on any OS/CI runner. The end-to-end scenarios that need
//! real file-level DB inspection (the M5c residue-scrub byte grep) live in the external
//! integration test `tests/credential_store.rs` instead.
use super::*;
use haily_tools::connector::CredentialGetter;

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

/// M6b (Activate-and-Measure phase 4b): the FALSE premise this phase corrects is that
/// headless "keeps a migrated connector working" via the DB-read path alone — once a
/// secret is migrated, its DB row holds only the marker, and `read_db_plaintext` never
/// returns a marker as a secret. This test simulates EXACTLY that state (marker present,
/// no env source) and confirms headless returns `None`, proving the premise really was
/// false before M6b's fix — then the companion tests below prove the fix.
#[tokio::test]
async fn headless_after_migration_with_no_env_source_cannot_read_the_secret() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::headless()).await;

    meta::upsert_preference(&s.db, "test.migrated_no_env", &super::marker::keyring_marker("test.migrated_no_env"), "test")
        .await
        .unwrap();

    assert!(
        s.get_secret("test.migrated_no_env").await.unwrap().is_none(),
        "headless with no env/file source and a migrated (marker-only) row must be None, \
         never the marker string itself"
    );
}

/// M6b: the per-connector env var (`HAILY_CRED__<CRED_REF_UPPER_SNAKE>`) is resolved
/// BEFORE the marker check — a headless boot recovers a migrated secret even though its
/// DB row holds only the marker.
#[tokio::test]
async fn headless_reads_per_connector_env_var_before_the_marker_check() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::headless()).await;

    let cred_ref = "connector.odoo.api_key";
    meta::upsert_preference(&s.db, cred_ref, &super::marker::keyring_marker(cred_ref), "test")
        .await
        .unwrap();

    std::env::set_var("HAILY_CRED__CONNECTOR_ODOO_API_KEY", "sk-from-env-var");
    let result = s.get_secret(cred_ref).await.unwrap();
    std::env::remove_var("HAILY_CRED__CONNECTOR_ODOO_API_KEY");

    assert_eq!(
        result.as_deref(),
        Some("sk-from-env-var"),
        "the per-connector env var must resolve BEFORE the DB marker check blinds the read"
    );
}

/// M6b: `HAILY_CRED_FILE` (a JSON object `{cred_ref: secret}`) is checked first, ahead of
/// even the per-connector env var, and — like the env var — before the marker check.
#[tokio::test]
async fn headless_reads_cred_file_before_the_marker_check() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::headless()).await;

    let cred_ref = "connector.custom.api_key";
    meta::upsert_preference(&s.db, cred_ref, &super::marker::keyring_marker(cred_ref), "test")
        .await
        .unwrap();

    let cred_file = dir.path().join("creds.json");
    std::fs::write(&cred_file, format!(r#"{{"{cred_ref}":"sk-from-file"}}"#)).unwrap();
    std::env::set_var("HAILY_CRED_FILE", &cred_file);
    let result = s.get_secret(cred_ref).await.unwrap();
    std::env::remove_var("HAILY_CRED_FILE");

    assert_eq!(
        result.as_deref(),
        Some("sk-from-file"),
        "HAILY_CRED_FILE must resolve BEFORE the DB marker check blinds the read"
    );
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

/// M6c: simulates a crash/SQLITE_BUSY that landed the marker write but never reached the
/// scrub — the marker is present but `SCRUB_CONFIRMED_PREF` is not. The NEXT `migrate_from_db`
/// call (next boot) must still run the scrub and confirm it, rather than short-circuiting on
/// the marker's own presence forever (the exact bug this fix closes).
#[tokio::test]
async fn crash_between_marker_and_scrub_heals_on_next_boot() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::default()).await;

    // Simulate the marker having ALREADY landed (as if `set_secret` + the marker overwrite
    // succeeded) but the process crashed before `scrub_residue`/the confirmation flag ran.
    meta::upsert_preference(&s.db, "test.crash", &super::marker::keyring_marker("test.crash"), "test")
        .await
        .unwrap();
    assert!(
        meta::get_preference(&s.db, SCRUB_CONFIRMED_PREF).await.unwrap().is_none(),
        "sanity: scrub confirmation absent before healing"
    );

    // Next boot's migrate_from_db call must heal it: run the scrub and confirm it, even
    // though the DB row already holds the marker (no fresh plaintext to migrate).
    s.migrate_from_db("test.crash").await.unwrap();

    assert_eq!(
        meta::get_preference(&s.db, SCRUB_CONFIRMED_PREF).await.unwrap().as_deref(),
        Some("true"),
        "the interrupted scrub must be re-run and confirmed on the next boot"
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

/// Safe Operator Harness phase 2: `HttpExecutor` is injected an `Arc<dyn CredentialGetter>`,
/// never a concrete `CredentialStore` — this proves the trait-object seam actually delegates
/// to the SAME read path (cache → keyring → policy-gated DB fallback) as calling the
/// inherent method directly, not a divergent or infinitely-recursive one.
#[tokio::test]
async fn credential_store_as_trait_object_delegates_to_the_same_read_path() {
    use_mock_backend();
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), CredentialPolicy::default()).await;
    s.set_secret("test.trait_object", "via-trait-object").await.unwrap();

    let getter: Arc<dyn CredentialGetter> = Arc::new(s);
    assert_eq!(
        getter.get_secret("test.trait_object").await.unwrap().as_deref(),
        Some("via-trait-object")
    );
    // An unconfigured reference is `Ok(None)` through the trait object too, not an error.
    assert!(getter.get_secret("test.never_configured").await.unwrap().is_none());
}
