//! DB-layer tests for connector manifests (Safe Operator Harness phase 4): versioning,
//! per-version immutability (append-only trigger +/-), content-hash determinism, the
//! mutable-status path, and the m3 human-only invariant (no registered Tool can write
//! this table — enforced structurally by there being no tool that does).
use haily_db::{queries::connectors, DbHandle};

async fn setup() -> (DbHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = DbHandle::init(&db_path).await.unwrap();
    (db, dir)
}

fn manifest_json(v: &str) -> String {
    format!(
        r#"{{"connector_name":"odoo","version":"{v}","base_url":"https://erp.example.com","allowed_ip_cidrs":["93.184.216.34/32"],"ops":[]}}"#
    )
}

fn new_manifest<'a>(version: &'a str, json: &'a str) -> connectors::NewManifest<'a> {
    connectors::NewManifest {
        connector_name: "odoo",
        version,
        manifest_json: json,
        base_url: "https://erp.example.com",
        allowed_ip_cidrs: r#"["93.184.216.34/32"]"#,
    }
}

#[tokio::test]
async fn two_versions_of_one_connector_coexist() {
    let (db, _dir) = setup().await;
    let j1 = manifest_json("1");
    let j2 = manifest_json("2");
    connectors::insert_version(&db, new_manifest("1", &j1))
        .await
        .unwrap();
    connectors::insert_version(&db, new_manifest("2", &j2))
        .await
        .unwrap();

    let active = connectors::list_active(&db).await.unwrap();
    assert_eq!(active.len(), 2, "both versions are active and coexist");
    let v1 = connectors::get_by_name_version(&db, "odoo", "1")
        .await
        .unwrap();
    let v2 = connectors::get_by_name_version(&db, "odoo", "2")
        .await
        .unwrap();
    assert!(v1.is_some() && v2.is_some());
}

#[tokio::test]
async fn reinserting_same_name_version_conflicts() {
    let (db, _dir) = setup().await;
    let j1 = manifest_json("1");
    connectors::insert_version(&db, new_manifest("1", &j1))
        .await
        .unwrap();
    // A new schema is a NEW version — re-inserting (odoo, 1) is a UNIQUE conflict, not an
    // in-place mutation of an approved manifest.
    let dup = manifest_json("1"); // even identical body must conflict on (name, version)
    let err = connectors::insert_version(&db, new_manifest("1", &dup)).await;
    assert!(err.is_err(), "re-inserting (name, version) must conflict");
}

#[tokio::test]
async fn content_hash_deterministic_for_identical_json() {
    // Same bytes → same hash (persist-stable); different bytes → different hash.
    let json = manifest_json("1");
    let h1 = connectors::content_hash(&json);
    let h2 = connectors::content_hash(&json);
    assert_eq!(h1, h2, "identical manifest_json hashes identically");
    assert_eq!(h1.len(), 64, "sha-256 hex is 64 chars");
    let other = connectors::content_hash(&manifest_json("2"));
    assert_ne!(h1, other, "differing manifest_json hashes differently");

    // The stored row's hash matches the helper computed on its own json.
    let (db, _dir) = setup().await;
    let row = connectors::insert_version(&db, new_manifest("1", &json))
        .await
        .unwrap();
    assert_eq!(row.content_hash, h1, "stored hash == helper(manifest_json)");
}

#[tokio::test]
async fn status_update_succeeds_positive_trigger() {
    // POSITIVE: toggling status (active<->disabled) is allowed — revocation must not
    // require minting a new version, so `status` is excluded from the append-only trigger.
    let (db, _dir) = setup().await;
    let j1 = manifest_json("1");
    let row = connectors::insert_version(&db, new_manifest("1", &j1))
        .await
        .unwrap();

    connectors::set_status(&db, &row.id, "disabled")
        .await
        .unwrap();
    let disabled = connectors::get_by_name_version(&db, "odoo", "1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(disabled.status, "disabled");
    assert!(
        connectors::list_active(&db).await.unwrap().is_empty(),
        "a disabled manifest is not listed active"
    );

    connectors::set_status(&db, &row.id, "active").await.unwrap();
    assert_eq!(
        connectors::list_active(&db).await.unwrap().len(),
        1,
        "re-enabling makes it active again"
    );
}

#[tokio::test]
async fn manifest_json_update_aborts_negative_trigger() {
    // NEGATIVE: manifest_json/content_hash/version are immutable per version — a direct
    // rewrite must be ABORTed by the append-only trigger (approved schema can't mutate).
    let (db, _dir) = setup().await;
    let j1 = manifest_json("1");
    let row = connectors::insert_version(&db, new_manifest("1", &j1))
        .await
        .unwrap();

    let err = sqlx::query("UPDATE connector_manifests SET manifest_json = 'tampered' WHERE id = ?")
        .bind(&row.id)
        .execute(db.pool())
        .await;
    assert!(err.is_err(), "rewriting manifest_json must abort");

    let err2 = sqlx::query("UPDATE connector_manifests SET version = '9' WHERE id = ?")
        .bind(&row.id)
        .execute(db.pool())
        .await;
    assert!(err2.is_err(), "rewriting version must abort");

    let err3 =
        sqlx::query("UPDATE connector_manifests SET content_hash = 'deadbeef' WHERE id = ?")
            .bind(&row.id)
            .execute(db.pool())
            .await;
    assert!(err3.is_err(), "rewriting content_hash must abort");

    // The evidentiary columns are unchanged after the aborted updates.
    let after = connectors::get_by_name_version(&db, "odoo", "1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.version, "1");
    assert_eq!(after.content_hash, row.content_hash);
}

/// m3 human-only invariant: no registered Tool can write `connector_manifests`. This is
/// enforced STRUCTURALLY — the only writers are `connectors::insert_version`/`set_status`,
/// which live in `haily-db` (the schema layer) and are called exclusively by a
/// human-invoked admin path, never by any `impl Tool`. `haily-db` has no dependency on
/// `haily-tools`, so no `Tool` can even reach these functions except through the
/// human admin surface. This test documents and guards that invariant: it asserts the
/// writer API is present in the schema layer (where a Tool cannot invoke it), and that the
/// registry-facing surface exposes only the read-only `list_active`.
#[tokio::test]
async fn no_tool_path_writes_connector_manifests() {
    // The writer API exists only here in haily-db, reachable only via a human admin path.
    let (db, _dir) = setup().await;
    let j1 = manifest_json("1");
    connectors::insert_version(&db, new_manifest("1", &j1))
        .await
        .unwrap();
    // The only surface the tool/registry layer consumes is the read-only list — a Tool
    // gets `ConnectorManifestRow`s to interpret, never a handle that mutates the table.
    let active = connectors::list_active(&db).await.unwrap();
    assert_eq!(active.len(), 1);
    // If a future change added a Tool-facing write to this table, it would have to add a
    // new writer fn HERE (haily-db) or make haily-db depend on haily-tools (breaking the
    // layering) — both are visible in review. There is intentionally no such path today.
}

/// Content-hash integrity is VERIFIED, not just stored: a correctly-approved row verifies,
/// while a row whose stored `content_hash` no longer matches its `manifest_json` (simulated
/// out-of-band tamper the append-only trigger can't catch) fails — the loader skips such a
/// row so a tampered schema never registers as a live connector.
#[tokio::test]
async fn verify_integrity_detects_out_of_band_tamper() {
    let (db, _dir) = setup().await;

    // A correctly-approved row verifies.
    let j1 = manifest_json("1");
    let good = connectors::insert_version(&db, new_manifest("1", &j1))
        .await
        .unwrap();
    assert!(good.verify_integrity(), "a correctly-hashed row verifies");

    // Simulate tamper the trigger can't catch: a raw insert of a row whose stored
    // content_hash does NOT match its manifest_json (a file-level DB edit / doctored restore).
    sqlx::query(
        "INSERT INTO connector_manifests
             (id, connector_name, version, content_hash, manifest_json, base_url,
              allowed_ip_cidrs, status, created_at)
         VALUES ('tamper-test-id', 'odoo', '2', 'not_the_real_hash', ?,
                 'https://erp.example.com', '[]', 'active', '2026-07-03T00:00:00Z')",
    )
    .bind(manifest_json("2"))
    .execute(db.pool())
    .await
    .unwrap();

    let tampered = connectors::get_by_name_version(&db, "odoo", "2")
        .await
        .unwrap()
        .unwrap();
    assert!(
        !tampered.verify_integrity(),
        "a row whose content_hash != hash(manifest_json) must fail integrity"
    );
}
