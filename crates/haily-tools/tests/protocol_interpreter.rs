//! Integration tests for Phase 3 — `HttpExecutor`'s declarative `protocol` interpreter.
//!
//! Uses a raw `TcpListener` fixture server (same pattern as `http_executor_auth.rs`/
//! `odoo_golden.rs`) so these prove the REAL wire body/redirect path, not a re-implementation
//! of it. `#[path]`-includes the M5b frozen fixtures (`fixtures/odoo_wire_fixtures.rs`),
//! HAND-VERIFIED snapshots of `OdooExecutor`'s actual wire behavior captured BEFORE its Phase
//! 4a retirement — these tests assert the generic interpreter reproduces THOSE frozen values,
//! not a live comparison against `OdooExecutor` (deleted; see `odoo_golden.rs`'s own doc
//! comment for the end-to-end live-sandbox proof).
//!
//! Coverage: envelope/arg-shaping parity between the generic interpreter and the frozen
//! `OdooExecutor` fixtures for create + write + unlink (M5b); fault-token parity (offline,
//! the frozen `odoo_fault::classify` vs the declarative `protocol::fault::classify_fault`);
//! read-back domain parity (offline); unresolvable model/method/token fail closed; M3 —
//! a reflected secret is scrubbed from the journal; M4 — `db`/`uid` come from the connection
//! overlay, never the hashed manifest; C1 — the envelope `{{key}}` token is dropped (emptied)
//! on a cross-host redirect hop, mirroring the header/query-param auth behavior.
#[path = "fixtures/odoo_wire_fixtures.rs"]
mod fixtures;

use async_trait::async_trait;
use haily_db::queries::{connectors, journal};
use haily_db::DbHandle;
use haily_tools::connector::odoo_fault::{self, FaultClass, OdooFault};
use haily_tools::connector::protocol;
use haily_tools::connector::{
    ConnectionOverlay, ConnectorExecutor, CredentialGetter, HttpConnectorTool, HttpExecutor,
    HttpExecutorConfig, Manifest,
};
use haily_tools::{Tool, ToolContext};
use haily_types::ApprovalGate;
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CRED_REF: &str = "connector.odoo.api_key";

// ---- shared fixture-server plumbing (mirrors http_executor_auth.rs) ----------------------

async fn bind_loopback(host: &str) -> (TcpListener, String) {
    let listener = TcpListener::bind(format!("{host}:0")).await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    (listener, format!("http://{addr}"))
}

/// Accept exactly `responses.len()` connections in order, capturing the raw request bytes
/// BEFORE replying, so every entry is present by the time the client's `.await` resolves.
fn serve_sequence(listener: TcpListener, responses: Vec<String>) -> Arc<StdMutex<Vec<String>>> {
    let captured = Arc::new(StdMutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);
    tokio::spawn(async move {
        for resp in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 16384];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            captured_clone.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_string());
            socket.write_all(resp.as_bytes()).await.expect("write response");
            socket.shutdown().await.ok();
        }
    });
    captured
}

fn ok_json(body: &Value) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{body}"
    )
}

fn status_json(status: u16, reason: &str, body: &str) -> String {
    format!("HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{body}")
}

fn redirect_to(location: &str) -> String {
    format!("HTTP/1.1 302 Found\r\nlocation: {location}\r\nconnection: close\r\n\r\n")
}

/// Split a raw captured HTTP request into its JSON body and parse it.
fn request_json_body(raw: &str) -> Value {
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    serde_json::from_str(body).unwrap_or_else(|e| panic!("captured request body not JSON: {e}\n{body}"))
}

struct FixedCredentialGetter {
    cred_ref: &'static str,
    secret: &'static str,
}

#[async_trait]
impl CredentialGetter for FixedCredentialGetter {
    async fn get_secret(&self, cred_ref: &str) -> anyhow::Result<Option<String>> {
        Ok((cred_ref == self.cred_ref).then(|| self.secret.to_string()))
    }
}

fn getter() -> Arc<FixedCredentialGetter> {
    Arc::new(FixedCredentialGetter { cred_ref: CRED_REF, secret: fixtures::KEY })
}

// ---- manifest builders ---------------------------------------------------------------------

/// A v1 (no auth/protocol) manifest carrying the ops `OdooExecutor` interprets natively.
fn odoo_shaped_ops(base_url: &str, cidr: &str) -> Value {
    json!({
        "connector_name": "odoo-crm-test",
        "version": "1",
        "base_url": base_url,
        "allowed_ip_cidrs": [cidr],
        "ops": [
            {"name": "odoo_contact_create", "model": "res.partner", "method": "create",
             "risk_tier": "ReversibleWrite", "compensability": "compensatable",
             "correlation_field": "ref",
             "compensation": {"op": "archive", "model": "res.partner", "method": "write", "values": {"active": false}}},
            {"name": "odoo_contact_update", "model": "res.partner", "method": "write",
             "risk_tier": "ReversibleWrite", "compensability": "compensatable",
             "correlation_field": "ref",
             "compensation": {"op": "write", "model": "res.partner", "method": "write"}}
        ]
    })
}

/// The SAME ops, PLUS `auth` + a `protocol` section built to reproduce `execute_kw` exactly.
fn v2_manifest_json(base_url: &str, cidr: &str) -> Value {
    let mut m = odoo_shaped_ops(base_url, cidr);
    m["auth"] = json!({"scheme": "header", "cred_ref": CRED_REF, "header_name": "X-Ignored"});
    m["protocol"] = json!({
        "endpoint_suffix": "/jsonrpc",
        "envelope": {
            "jsonrpc": "2.0", "method": "call", "id": null,
            "params": {
                "service": "object", "method": "execute_kw",
                "args": ["{{db}}", "{{uid}}", "{{key}}", "{{model}}", "{{method}}", "{{args}}", "{{kwargs}}"]
            }
        },
        "methods": [
            {"method": "create", "arg_template": ["{{values}}"]},
            {"method": "write", "arg_template": ["{{ids}}", "{{values}}"]},
            {"method": "unlink", "arg_template": ["{{ids}}"]}
        ],
        "fault_rules": [
            {"match_field": "name", "match_value": "odoo.exceptions.AccessError", "normalized": "AccessError"},
            {"match_field": "name", "match_value": "odoo.exceptions.ValidationError", "normalized": "ValidationError"},
            {"match_field": "name", "match_value": "odoo.exceptions.MissingError", "normalized": "MissingError"}
        ],
        "readback": {"locate_by": "id", "active_test": true, "unwrap_first": true},
        "context": {"lang": fixtures::LANG, "tz": fixtures::TZ},
        "prevalidate": [{"model": "res.partner", "required_fields": ["name"]}]
    });
    m
}

fn v2_manifest(base_url: &str, cidr: &str) -> Manifest {
    haily_tools::connector::manifest::parse(&v2_manifest_json(base_url, cidr).to_string())
        .expect("v2 manifest parses")
}

fn generic_executor_for(manifest: Arc<Manifest>, overlay: Option<ConnectionOverlay>) -> HttpExecutor {
    let mut cfg = HttpExecutorConfig::production(manifest, Arc::new(AtomicBool::new(false)), Duration::from_secs(5))
        .with_credential_getter(Some(getter()))
        .with_connection_overlay(overlay.or_else(|| {
            Some(ConnectionOverlay {
                db: Some(fixtures::DB.to_string()),
                uid: Some(fixtures::UID),
                ..Default::default()
            })
        }));
    cfg.allow_loopback = true; // TEST ONLY.
    HttpExecutor::new(cfg)
}

// ---- M5b: envelope + arg-shaping parity (generic interpreter vs the FROZEN OdooExecutor
// ---- fixture — OdooExecutor itself is retired, Phase 4a; the fixture is its hand-verified
// ---- snapshot, captured before deletion, and is the parity oracle from here on). ----------

#[tokio::test]
async fn create_envelope_matches_frozen_fixture() {
    let (listener_generic, url_generic) = bind_loopback("127.0.0.1").await;
    let captured_generic = serve_sequence(listener_generic, vec![ok_json(&json!({"jsonrpc": "2.0", "id": null, "result": 42}))]);
    let generic = generic_executor_for(Arc::new(v2_manifest(&url_generic, "127.0.0.1/32")), None);
    generic
        .call("odoo_contact_create", &json!({"correlation_ref": "corr-1", "values": {"name": "Alice"}}))
        .await
        .expect("generic create succeeds");

    let expected = fixtures::execute_kw_envelope(
        fixtures::DB,
        fixtures::UID,
        fixtures::KEY,
        "res.partner",
        "create",
        fixtures::contact_create_args("Alice", "corr-1"),
        fixtures::call_kwargs(),
    );
    let generic_body = request_json_body(&captured_generic.lock().unwrap()[0]);
    assert_eq!(generic_body, expected, "the generic interpreter must reproduce OdooExecutor's frozen body exactly (M5b)");
}

#[tokio::test]
async fn write_envelope_matches_frozen_fixture() {
    let (listener_generic, url_generic) = bind_loopback("127.0.0.1").await;
    let captured_generic = serve_sequence(listener_generic, vec![ok_json(&json!({"jsonrpc": "2.0", "id": null, "result": true}))]);
    let generic = generic_executor_for(Arc::new(v2_manifest(&url_generic, "127.0.0.1/32")), None);
    generic
        .call("odoo_contact_update", &json!({"ids": [7], "values": {"function": "after"}}))
        .await
        .expect("generic write succeeds");

    let expected = fixtures::execute_kw_envelope(
        fixtures::DB,
        fixtures::UID,
        fixtures::KEY,
        "res.partner",
        "write",
        fixtures::contact_update_args(7, json!({"function": "after"})),
        fixtures::call_kwargs(),
    );
    assert_eq!(request_json_body(&captured_generic.lock().unwrap()[0]), expected);
}

#[tokio::test]
async fn unlink_envelope_matches_frozen_fixture() {
    // A bare compensation-op keyword (not a manifest op NAME) — model/method travel on the
    // plan/params, exactly as `journal_undo` drives a real compensation call.
    let plan = json!({"model": "mail.activity", "method": "unlink", "ids": [5]});

    let (listener_generic, url_generic) = bind_loopback("127.0.0.1").await;
    let captured_generic = serve_sequence(listener_generic, vec![ok_json(&json!({"jsonrpc": "2.0", "id": null, "result": true}))]);
    let generic = generic_executor_for(Arc::new(v2_manifest(&url_generic, "127.0.0.1/32")), None);
    generic.call("unlink", &plan).await.expect("generic unlink succeeds");

    let expected = fixtures::execute_kw_envelope(
        fixtures::DB,
        fixtures::UID,
        fixtures::KEY,
        "mail.activity",
        "unlink",
        fixtures::unlink_args(5),
        fixtures::call_kwargs(),
    );
    assert_eq!(request_json_body(&captured_generic.lock().unwrap()[0]), expected);
}

// ---- M5b: fault-token parity (offline — both sides vs. the frozen fixture) ----------------

#[test]
fn fault_token_parity_generic_interpreter_vs_odoo_classify() {
    let fault_rules: Vec<haily_tools::connector::FaultRule> = vec![
        haily_tools::connector::FaultRule { match_field: "name".into(), match_value: "odoo.exceptions.AccessError".into(), normalized: "AccessError".into() },
        haily_tools::connector::FaultRule { match_field: "name".into(), match_value: "odoo.exceptions.ValidationError".into(), normalized: "ValidationError".into() },
        haily_tools::connector::FaultRule { match_field: "name".into(), match_value: "odoo.exceptions.MissingError".into(), normalized: "MissingError".into() },
    ];
    for (name, expected_token) in fixtures::FAULT_TOKENS {
        let fault = OdooFault { code: Some("200".into()), name: Some((*name).to_string()), fault_string: "human text".into() };
        let odoo_class = odoo_fault::classify(&fault);
        let odoo_token = match odoo_class {
            FaultClass::NonRetryableAccess => "AccessError",
            FaultClass::RetryableValidation => "ValidationError",
            FaultClass::StaleReference => "MissingError",
            FaultClass::Unknown => "UnknownError",
        };
        assert_eq!(odoo_token, *expected_token, "OdooExecutor's own classification for {name}");
        let generic_token = protocol::fault::classify_fault(&fault_rules, &fault, None);
        assert_eq!(generic_token, *expected_token, "generic interpreter classification for {name}");
    }
}

// ---- M5b: read-back domain parity (offline) -----------------------------------------------

#[test]
fn readback_domain_parity_with_frozen_fixture() {
    assert_eq!(protocol::readback::build_domain(Some("42"), Some("ref"), "corr-1"), fixtures::readback_domain_by_id(42));
    assert_eq!(
        protocol::readback::build_domain(None, Some("ref"), "corr-1"),
        fixtures::readback_domain_by_correlation("ref", "corr-1")
    );
    assert_eq!(protocol::readback::build_domain(None, None, ""), fixtures::readback_domain_empty());
}

// ---- M5b: read_back end-to-end (search_read envelope + first-record unwrap) ---------------

#[tokio::test]
async fn read_back_by_id_matches_frozen_fixture_and_unwraps_first_record() {
    let search_read_result = json!({"jsonrpc": "2.0", "id": null, "result": [{"id": 42, "name": "Alice", "write_date": "2026-07-06 00:00:00"}]});

    let (listener_generic, url_generic) = bind_loopback("127.0.0.1").await;
    let captured_generic = serve_sequence(listener_generic, vec![ok_json(&search_read_result)]);
    let generic = generic_executor_for(Arc::new(v2_manifest(&url_generic, "127.0.0.1/32")), None);
    let generic_record = generic
        .read_back("odoo_contact_read", "", None, Some("42"))
        .await
        .expect("generic read_back succeeds");

    // Unwraps the one-element search_read array to the bare record object (C10: write_date
    // exposed for the undo version guard) — the SAME shape `OdooExecutor::read_back` produced.
    assert_eq!(generic_record, json!({"id": 42, "name": "Alice", "write_date": "2026-07-06 00:00:00"}));

    let expected_body = fixtures::execute_kw_envelope(
        fixtures::DB,
        fixtures::UID,
        fixtures::KEY,
        "res.partner",
        "search_read",
        fixtures::readback_domain_by_id(42),
        json!({"limit": 1, "context": {"lang": fixtures::LANG, "tz": fixtures::TZ, "active_test": false}}),
    );
    assert_eq!(request_json_body(&captured_generic.lock().unwrap()[0]), expected_body, "the generic interpreter must reproduce OdooExecutor's frozen read-back body exactly (M5b)");
}

// ---- Unresolvable model/method/token fail closed -------------------------------------------

#[tokio::test]
async fn unresolvable_manifest_op_fails_closed_no_request_sent() {
    let (listener, url) = bind_loopback("127.0.0.1").await;
    let captured = serve_sequence(listener, vec![ok_json(&json!({"result": 1}))]);
    let generic = generic_executor_for(Arc::new(v2_manifest(&url, "127.0.0.1/32")), None);

    // "no_such_op" is neither a manifest op name NOR a compensation keyword with a model on
    // its params — model/method resolution must fail BEFORE any network call.
    let result = generic.call("no_such_op", &json!({})).await;
    assert!(result.is_err(), "unresolvable op must fail closed");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(captured.lock().unwrap().is_empty(), "no request may leave the executor");
}

#[tokio::test]
async fn envelope_referencing_an_unknown_token_fails_closed_no_request_sent() {
    let (listener, url) = bind_loopback("127.0.0.1").await;
    let captured = serve_sequence(listener, vec![ok_json(&json!({"result": 1}))]);
    let mut manifest_json = v2_manifest_json(&url, "127.0.0.1/32");
    manifest_json["protocol"]["envelope"] = json!({"ghost": "{{totally_unresolvable_token}}"});
    let manifest = haily_tools::connector::manifest::parse(&manifest_json.to_string()).unwrap();
    let generic = generic_executor_for(Arc::new(manifest), None);

    let result = generic
        .call("odoo_contact_create", &json!({"correlation_ref": "c", "values": {"name": "Alice"}}))
        .await;
    assert!(result.is_err(), "an unresolvable envelope token must fail closed");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(captured.lock().unwrap().is_empty(), "no request may leave the executor");
}

// ---- M3: a reflected secret is scrubbed before it reaches the journal --------------------

struct NoopGate;
#[async_trait]
impl ApprovalGate for NoopGate {
    async fn request(&self, _approval_id: uuid::Uuid, _session_id: uuid::Uuid, _cancel: &tokio_util::sync::CancellationToken) -> bool {
        false
    }
}

/// A throwaway view sink (stores nothing) — these tests never publish a view; it exists
/// only because `ToolContext` requires a handle.
struct NoopViewSink;
impl haily_types::ViewSink for NoopViewSink {
    fn insert(&self, _view: haily_types::DataView) -> uuid::Uuid {
        uuid::Uuid::nil()
    }
}

async fn tool_ctx(db: Arc<DbHandle>) -> (ToolContext, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let kms_db = DbHandle::init(&dir.path().join("kms.db")).await.unwrap();
    let kms = Arc::new(haily_kms::KmsHandle::init(kms_db, dir.path()).await.unwrap());
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let ctx = ToolContext {
        db,
        kms,
        session_id: uuid::Uuid::new_v4(),
        turn_id: uuid::Uuid::new_v4(),
        depth: 0,
        domain: None,
        approval_gate: Arc::new(NoopGate),
        approval_tx: tx,
        cancel: tokio_util::sync::CancellationToken::new(),
        turn_deletes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        last_journal_id: Arc::new(std::sync::Mutex::new(None)),
        run_id: None,
        depth_mode: haily_types::DepthMode::Normal,
        view_sink: Arc::new(NoopViewSink),
    };
    (ctx, dir)
}

#[tokio::test]
async fn reflected_secret_in_fault_body_is_scrubbed_before_journaling() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());

    let (listener, base_url) = bind_loopback("127.0.0.1").await;
    // The server reflects the bearer secret back in a 401 body — the scenario M3 targets.
    let reflected = format!(r#"{{"error":"invalid key {} rejected"}}"#, fixtures::KEY);
    let _captured = serve_sequence(listener, vec![status_json(401, "Unauthorized", &reflected)]);

    let manifest_json = json!({
        "connector_name": "reflect-test",
        "version": "1",
        "base_url": base_url,
        "allowed_ip_cidrs": ["127.0.0.1/32"],
        "ops": [{"name": "reflect_op", "risk_tier": "IrreversibleWrite",
                 "compensability": "compensatable", "compensation": {"op": "unlink"}}],
        "auth": {"scheme": "bearer", "cred_ref": CRED_REF}
    });
    let manifest = Arc::new(haily_tools::connector::manifest::parse(&manifest_json.to_string()).unwrap());
    let op = Arc::new(manifest.ops[0].clone());
    let mut cfg = HttpExecutorConfig::production(Arc::clone(&manifest), Arc::new(AtomicBool::new(false)), Duration::from_secs(5))
        .with_credential_getter(Some(getter()));
    cfg.allow_loopback = true;
    let executor: Arc<dyn ConnectorExecutor> = Arc::new(HttpExecutor::new(cfg));

    let tool = HttpConnectorTool {
        manifest,
        op,
        executor,
        kill: Arc::new(AtomicBool::new(false)),
        cred_ref: CRED_REF.to_string(),
        manifest_hash: "test-hash".to_string(),
    };
    let (ctx, _kd) = tool_ctx(Arc::clone(&db)).await;
    let result = tool.execute(json!({"params": {"correlation_ref": "c1", "values": {}}}), &ctx).await;
    assert!(result.is_err(), "a fault must surface as an error to the caller");

    let row = journal::list_by_session(&db, &ctx.session_id.to_string())
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("outbox row recorded before the call");
    let all = format!(
        "{}{}{}",
        row.request_params,
        row.pre_state.clone().unwrap_or_default(),
        row.post_state.clone().unwrap_or_default(),
    );
    assert!(!all.contains(fixtures::KEY), "M3: reflected secret must never reach the journal: {all}");
}

// ---- M4: db/uid come from the connection overlay, never the hashed manifest --------------

#[tokio::test]
async fn overlay_supplies_uid_independently_of_the_hashed_manifest() {
    let manifest_json = v2_manifest_json("http://placeholder.invalid", "127.0.0.1/32").to_string();
    let hash_before = connectors::content_hash(&manifest_json);
    let manifest = Arc::new(haily_tools::connector::manifest::parse(&manifest_json).unwrap());

    let (listener_a, url_a) = bind_loopback("127.0.0.1").await;
    let captured_a = serve_sequence(listener_a, vec![ok_json(&json!({"jsonrpc": "2.0", "id": null, "result": 1}))]);
    let overlay_a = ConnectionOverlay {
        base_url_override: Some(url_a),
        db: Some(fixtures::DB.to_string()),
        uid: Some(100),
        cred_ref_override: None,
    };
    let exec_a = generic_executor_for(Arc::clone(&manifest), Some(overlay_a));
    exec_a
        .call("odoo_contact_create", &json!({"correlation_ref": "c", "values": {"name": "Alice"}}))
        .await
        .expect("call via overlay A succeeds");

    let (listener_b, url_b) = bind_loopback("127.0.0.1").await;
    let captured_b = serve_sequence(listener_b, vec![ok_json(&json!({"jsonrpc": "2.0", "id": null, "result": 1}))]);
    let overlay_b = ConnectionOverlay {
        base_url_override: Some(url_b),
        db: Some(fixtures::DB.to_string()),
        uid: Some(200),
        cred_ref_override: None,
    };
    let exec_b = generic_executor_for(Arc::clone(&manifest), Some(overlay_b));
    exec_b
        .call("odoo_contact_create", &json!({"correlation_ref": "c", "values": {"name": "Alice"}}))
        .await
        .expect("call via overlay B succeeds");

    let uid_a = request_json_body(&captured_a.lock().unwrap()[0])["params"]["args"][1].clone();
    let uid_b = request_json_body(&captured_b.lock().unwrap()[0])["params"]["args"][1].clone();
    assert_eq!(uid_a, json!(100), "overlay A's uid must reach the wire body");
    assert_eq!(uid_b, json!(200), "overlay B's uid must reach the wire body — proving the overlay, not a manifest field, drives it");

    // The manifest_json STRING never changed between the two calls — its content hash (the
    // approval unit) is untouched by a per-deployment uid difference (M4's headline guarantee).
    assert_eq!(connectors::content_hash(&manifest_json), hash_before);
}

// ---- C1 carryover: the envelope {{key}} token is dropped on a cross-host redirect hop -----

#[tokio::test]
async fn cross_host_redirect_drops_the_envelope_secret_c1() {
    let (listener_b, url_b) = bind_loopback("127.0.0.2").await;
    let captured_b = serve_sequence(listener_b, vec![ok_json(&json!({"jsonrpc": "2.0", "id": null, "result": 1}))]);

    let (listener_a, url_a) = bind_loopback("127.0.0.1").await;
    let captured_a = serve_sequence(listener_a, vec![redirect_to(&format!("{url_b}/jsonrpc"))]);

    // Both loopback literals must be pinned for the redirect target to pass the SSRF allowance.
    let mut manifest_json = v2_manifest_json(&url_a, "127.0.0.1/32");
    manifest_json["allowed_ip_cidrs"] = json!(["127.0.0.1/32", "127.0.0.2/32"]);
    let manifest = Arc::new(haily_tools::connector::manifest::parse(&manifest_json.to_string()).unwrap());

    let exec = generic_executor_for(manifest, None);
    exec.call("odoo_contact_create", &json!({"correlation_ref": "c", "values": {"name": "Alice"}}))
        .await
        .expect("call follows the redirect and succeeds");

    let body_a = request_json_body(&captured_a.lock().unwrap()[0]);
    let body_b = request_json_body(&captured_b.lock().unwrap()[0]);
    assert_eq!(body_a["params"]["args"][2], json!(fixtures::KEY), "the manifest's own host must carry the real key");
    assert_eq!(body_b["params"]["args"][2], json!(""), "a cross-host hop must receive an EMPTY key, never the secret (C1)");
}
