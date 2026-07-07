//! Integration tests for `haily_app::connector_config` (Phase 7, "Assistant Depth").
//!
//! Exercises the public API against a real (tempfile) `DbHandle` — the read side's
//! baseline-backfill + re-approval-diff behavior needs real manifest rows to be meaningful,
//! unlike the pure-function unit tests already covered in-crate.
use haily_app::connector_config::list_connectors;
use haily_db::queries::connectors::{self, NewManifest};
use haily_db::queries::meta;
use haily_db::DbHandle;

async fn setup() -> (DbHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
    (db, dir)
}

/// `auth.cred_ref` matters here: the M1 re-approval diff only compares `base_url` when an
/// `auth` section is present on either version (see `manifest::manifest_diff`'s doc).
fn manifest_json(version: &str, base_url: &str) -> String {
    format!(
        r#"{{"connector_name":"testconn","version":"{version}","base_url":"{base_url}",
             "allowed_ip_cidrs":["93.184.216.34/32"],
             "ops":[{{"name":"read_thing","risk_tier":"Read"}}],
             "auth":{{"scheme":"bearer","cred_ref":"connector.testconn.api_key"}}}}"#
    )
}

fn new_manifest<'a>(version: &'a str, json: &'a str, base_url: &'a str) -> NewManifest<'a> {
    NewManifest {
        connector_name: "testconn",
        version,
        manifest_json: json,
        base_url,
        allowed_ip_cidrs: r#"["93.184.216.34/32"]"#,
    }
}

#[tokio::test]
async fn first_view_backfills_baseline_and_reports_no_reapproval() {
    let (db, _dir) = setup().await;
    let json = manifest_json("1", "https://erp.example.com");
    connectors::insert_version(&db, new_manifest("1", &json, "https://erp.example.com"))
        .await
        .unwrap();

    let summaries = list_connectors(&db).await.unwrap();
    assert_eq!(summaries.len(), 1);
    let s = &summaries[0];
    assert_eq!(s.connector_name, "testconn");
    assert_eq!(s.version, "1");
    assert_eq!(s.risk_tier, "Read");
    assert_eq!(s.cred_ref.as_deref(), Some("connector.testconn.api_key"));
    assert!(s.reapproval.is_none(), "first-ever view must NOT flag a pre-existing connector");

    let pref_key = "connector.testconn.approved_version";
    assert_eq!(
        meta::get_preference(&db, pref_key).await.unwrap().as_deref(),
        Some("1"),
        "the live version must be silently adopted as the baseline"
    );
}

#[tokio::test]
async fn new_version_with_changed_base_url_surfaces_reapproval_diff() {
    let (db, _dir) = setup().await;
    let json1 = manifest_json("1", "https://erp.example.com");
    connectors::insert_version(&db, new_manifest("1", &json1, "https://erp.example.com"))
        .await
        .unwrap();
    // First view backfills the baseline at version 1.
    list_connectors(&db).await.unwrap();

    let json2 = manifest_json("2", "https://erp-new.example.com");
    connectors::insert_version(&db, new_manifest("2", &json2, "https://erp-new.example.com"))
        .await
        .unwrap();

    let summaries = list_connectors(&db).await.unwrap();
    assert_eq!(summaries.len(), 1, "still one connector — latest version wins the summary row");
    let s = &summaries[0];
    assert_eq!(s.version, "2");
    let reapproval = s.reapproval.as_ref().expect("a version bump with a base_url change must flag re-approval");
    assert_eq!(reapproval.approved_version, "1");
    assert_eq!(reapproval.live_version, "2");
    assert_eq!(
        reapproval.diff.base_url,
        Some(("https://erp.example.com".to_string(), "https://erp-new.example.com".to_string())),
        "M1: base_url change must surface in the diff because auth is present"
    );

    // Acknowledging clears the banner on the next read.
    haily_app::connector_config::acknowledge_connector_version(&db, "testconn", "2")
        .await
        .unwrap();
    let after_ack = list_connectors(&db).await.unwrap();
    assert!(after_ack[0].reapproval.is_none(), "acknowledging must clear the banner");
}

#[tokio::test]
async fn disabled_connector_still_appears_for_re_enabling() {
    let (db, _dir) = setup().await;
    let json = manifest_json("1", "https://erp.example.com");
    let row = connectors::insert_version(&db, new_manifest("1", &json, "https://erp.example.com"))
        .await
        .unwrap();
    connectors::set_status(&db, &row.id, "disabled").await.unwrap();

    let summaries = list_connectors(&db).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].status, "disabled");
}
