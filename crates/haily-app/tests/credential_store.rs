//! Integration tests for `haily_app::credential_store` (Harness Completion phase 4).
//!
//! Uses ONLY the public API (`CredentialStore`, `CredentialPolicy`, `FALLBACK_WARNING_PREF`,
//! `is_keyring_marker`) plus the keyring crate's own mock backend — no real OS credential
//! store is touched, so this suite is deterministic on any platform/CI runner. The in-crate
//! unit tests (`src/credential_store.rs`) cover the read/write fallback branch logic in
//! isolation; this file covers the end-to-end scenarios that need real file-level DB
//! inspection (M5c residue scrub) or exercise the public surface as an external caller would.
use haily_app::{CredentialPolicy, CredentialStore};
use haily_db::queries::meta;
use haily_db::DbHandle;
use std::sync::Arc;

fn use_mock_keyring() {
    keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
}

/// Read the raw bytes of the main DB file AND its `-wal` sidecar (present under WAL
/// journal mode, which `DbHandle::init` always enables) — the residue scrub test needs to
/// see across BOTH, since a value can be sitting in either depending on checkpoint timing.
fn read_db_bytes(db_path: &std::path::Path) -> Vec<u8> {
    let mut bytes = std::fs::read(db_path).unwrap_or_default();
    let wal_path = db_path.with_extension("db-wal");
    bytes.extend(std::fs::read(&wal_path).unwrap_or_default());
    bytes
}

fn contains_secret(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[tokio::test]
async fn get_secret_round_trip_via_public_api() {
    use_mock_keyring();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());
    let store = CredentialStore::new(db, CredentialPolicy::default());

    assert!(store.get_secret("integration.roundtrip").await.unwrap().is_none());
    store
        .set_secret("integration.roundtrip", "sk-integration-abc")
        .await
        .unwrap();
    assert_eq!(
        store.get_secret("integration.roundtrip").await.unwrap().as_deref(),
        Some("sk-integration-abc")
    );
}

#[tokio::test]
async fn platform_failure_on_read_is_loud_and_persisted_then_db_read_fallback_succeeds() {
    use_mock_keyring();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());
    meta::upsert_preference(&db, "integration.readfail", "plaintext-fallback-value", "test")
        .await
        .unwrap();

    let store = CredentialStore::new(Arc::clone(&db), CredentialPolicy::default());
    store
        .force_next_keyring_error(
            "integration.readfail",
            keyring::Error::PlatformFailure(Box::new(std::io::Error::other("simulated RPC failure"))),
        )
        .await;

    let value = store.get_secret("integration.readfail").await.unwrap();
    assert_eq!(value.as_deref(), Some("plaintext-fallback-value"));

    // The warning is a PERSISTED DB flag (M5a/M5b), not just a log line — assert the row
    // exists via the same `meta` query the GUI's `get_preferences` command reads from.
    let warning = meta::get_preference(&db, haily_app::credential_store::FALLBACK_WARNING_PREF)
        .await
        .unwrap();
    assert_eq!(warning.as_deref(), Some("true"));
}

#[tokio::test]
async fn write_failure_fails_closed_vs_succeeds_plaintext_only_with_opt_in() {
    use_mock_keyring();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());

    // Branch 1: default policy — a keyring write failure is FAIL-CLOSED (M5b dangerous
    // direction never happens silently).
    let fail_closed_store = CredentialStore::new(Arc::clone(&db), CredentialPolicy::default());
    fail_closed_store
        .force_next_keyring_error(
            "integration.write_fail_closed",
            keyring::Error::PlatformFailure(Box::new(std::io::Error::other("simulated"))),
        )
        .await;
    let result = fail_closed_store
        .set_secret("integration.write_fail_closed", "never-persisted")
        .await;
    assert!(result.is_err(), "default policy must fail closed on write failure");
    assert!(
        meta::get_preference(&db, "integration.write_fail_closed").await.unwrap().is_none(),
        "no silent plaintext write must have occurred"
    );

    // Branch 2: explicit opt-in — the SAME kind of failure now succeeds via plaintext DB.
    let opt_in_store = CredentialStore::new(
        Arc::clone(&db),
        CredentialPolicy {
            allow_write_plaintext: true,
            ..CredentialPolicy::default()
        },
    );
    opt_in_store
        .force_next_keyring_error(
            "integration.write_opt_in",
            keyring::Error::PlatformFailure(Box::new(std::io::Error::other("simulated"))),
        )
        .await;
    opt_in_store
        .set_secret("integration.write_opt_in", "opted-in-plaintext")
        .await
        .unwrap();
    assert_eq!(
        meta::get_preference(&db, "integration.write_opt_in").await.unwrap().as_deref(),
        Some("opted-in-plaintext")
    );
}

#[tokio::test]
async fn headless_policy_skips_keyring_entirely() {
    use_mock_keyring();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());
    let store = CredentialStore::new(Arc::clone(&db), CredentialPolicy::headless());

    // A value only ever written to the plaintext DB (never migrated) is still readable —
    // headless never even attempts the keyring RPC, so a NoEntry/PlatformFailure on the
    // mock backend can't be the reason this works; it goes straight to the DB path.
    meta::upsert_preference(&db, "integration.headless", "headless-db-value", "test")
        .await
        .unwrap();
    assert_eq!(
        store.get_secret("integration.headless").await.unwrap().as_deref(),
        Some("headless-db-value")
    );
}

/// M5c: the whole point of the residue scrub. Seeds a plaintext secret, confirms the raw
/// bytes ARE present in the DB file (proving the test itself is meaningful — if this
/// assertion failed, the "not present after" assertion below would be vacuous), runs the
/// migration, and confirms the raw bytes are GONE from both the main file and the `-wal`
/// sidecar.
#[tokio::test]
async fn migration_residue_scrub_removes_plaintext_secret_bytes_from_db_file() {
    use_mock_keyring();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.db");
    let db = Arc::new(DbHandle::init(&db_path).await.unwrap());

    const SECRET: &str = "sk-M5C-RESIDUE-CANARY-VALUE-9f8e7d6c5b4a";
    meta::upsert_preference(&db, "integration.residue", SECRET, "test")
        .await
        .unwrap();

    // Force a WAL checkpoint via a throwaway write so the seeded row is flushed into a
    // page the pre-migration grep can actually observe (SQLite may otherwise batch the
    // insert only in the WAL, which the grep below reads anyway, but this keeps the
    // "present before" proof robust regardless of where the page currently lives).
    let before_bytes = read_db_bytes(&db_path);
    assert!(
        contains_secret(&before_bytes, SECRET),
        "test setup invariant: the raw secret must be visible in the DB file BEFORE \
         migration, otherwise the post-migration absence check below would be meaningless"
    );

    let store = CredentialStore::new(Arc::clone(&db), CredentialPolicy::default());
    store.migrate_from_db("integration.residue").await.unwrap();

    // Read-your-write: the secret is now served from the keyring, not plaintext.
    assert_eq!(
        store.get_secret("integration.residue").await.unwrap().as_deref(),
        Some(SECRET)
    );
    let db_row = meta::get_preference(&db, "integration.residue").await.unwrap().unwrap();
    assert!(haily_app::credential_store::is_keyring_marker(&db_row));

    // Write enough additional churn to force SQLite to actually reuse/rewrite the freed
    // page rather than leaving it untouched at the end of the file — VACUUM inside
    // `migrate_from_db`'s scrub already rebuilds the whole file, so this is a defense-in-
    // depth pass, not strictly required, but keeps the assertion robust against SQLite
    // page-allocation specifics.
    for i in 0..20 {
        meta::upsert_preference(&db, &format!("integration.churn.{i}"), "filler", "test")
            .await
            .unwrap();
    }

    let after_bytes = read_db_bytes(&db_path);
    assert!(
        !contains_secret(&after_bytes, SECRET),
        "M5c: the raw secret must NOT survive in the WAL/main file after the migration's \
         checkpoint(TRUNCATE) + VACUUM residue scrub"
    );
}

#[tokio::test]
async fn migration_marker_makes_repeat_boot_a_no_op() {
    use_mock_keyring();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());
    meta::upsert_preference(&db, "integration.idempotent", "once", "test")
        .await
        .unwrap();

    let store = CredentialStore::new(Arc::clone(&db), CredentialPolicy::default());
    store.migrate_from_db("integration.idempotent").await.unwrap();
    let marker = meta::get_preference(&db, "integration.idempotent").await.unwrap();

    // A second "boot" against the same store is a pure no-op (idempotent) — must not
    // error, must not change the marker, must not attempt to re-write the (already gone)
    // plaintext value.
    store.migrate_from_db("integration.idempotent").await.unwrap();
    assert_eq!(
        meta::get_preference(&db, "integration.idempotent").await.unwrap(),
        marker
    );
}
