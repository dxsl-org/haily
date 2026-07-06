//! Integration tests for Safe Operator Harness phase 2 — `HttpExecutor` auth injection.
//!
//! Uses a raw `TcpListener` fixture server (same pattern as `redirect_ssrf.rs`/
//! `odoo_golden.rs`) rather than a mocking crate, so these prove the REAL redirect +
//! request-builder path, not a re-implementation of it.
//!
//! Coverage: bearer/header/query-param scheme shape; auth applied on the manifest's own
//! host across a same-host redirect; **C1** — auth DROPPED on a cross-host redirect hop
//! (the headline security requirement of this phase); fail-closed on an unresolvable
//! credential; v1 backward compat (no `auth` section → no header, even with a getter
//! injected); **m1** — a query-param secret never reaches the redirect diagnostic log even
//! when a same-host redirect echoes the query string back.
use async_trait::async_trait;
use haily_tools::connector::{
    manifest, ConnectorExecutor, CredentialGetter, HttpExecutor, HttpExecutorConfig, Manifest,
};
use serde_json::json;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A `CredentialGetter` returning a single fixed secret for one exact `cred_ref`, `Ok(None)`
/// for any other — enough to exercise both the success and the fail-closed paths.
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

fn bearer_getter() -> Arc<FixedCredentialGetter> {
    Arc::new(FixedCredentialGetter {
        cred_ref: "test.cred",
        secret: "SECRET123",
    })
}

/// Bind a fixture listener on `host` (an explicit loopback literal, e.g. `127.0.0.2`, so a
/// test can model a genuinely DIFFERENT host from the manifest's own `127.0.0.1` base_url)
/// and return it plus its `http://host:port` URL.
async fn bind_loopback(host: &str) -> (TcpListener, String) {
    let listener = TcpListener::bind(format!("{host}:0")).await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    (listener, format!("http://{addr}"))
}

/// Accept exactly `responses.len()` connections in order, replying with `responses[i]` to
/// the i-th, recording the raw bytes of each request BEFORE writing the reply — so by the
/// time the client's final `.await` resolves, every entry this hop chain touched is already
/// present (no race between assertion and capture).
fn serve_sequence(listener: TcpListener, responses: Vec<String>) -> Arc<StdMutex<Vec<String>>> {
    let captured = Arc::new(StdMutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);
    tokio::spawn(async move {
        for resp in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            captured_clone
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..n]).to_string());
            socket.write_all(resp.as_bytes()).await.expect("write response");
            socket.shutdown().await.ok();
        }
    });
    captured
}

fn ok_200() -> String {
    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{}".to_string()
}

fn redirect_to(location: &str) -> String {
    format!("HTTP/1.1 302 Found\r\nlocation: {location}\r\nconnection: close\r\n\r\n")
}

fn manifest_with_auth(base_url: &str, allowed_ip_cidrs: &[&str], auth_json: &str) -> Arc<Manifest> {
    let cidrs = allowed_ip_cidrs
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(",");
    let manifest_json = format!(
        r#"{{"connector_name":"test-connector","version":"1","base_url":"{base_url}",
            "allowed_ip_cidrs":[{cidrs}],"ops":[],"auth":{auth_json}}}"#
    );
    Arc::new(manifest::parse(&manifest_json).expect("test manifest parses"))
}

/// Build an executor with the TEST-ONLY loopback SSRF carve-out enabled (never true in
/// production — see `HttpExecutor::allow_loopback`) so it can reach the fixture servers
/// above.
fn loopback_executor(manifest: Arc<Manifest>, getter: Arc<FixedCredentialGetter>) -> HttpExecutor {
    let mut cfg = HttpExecutorConfig::production(
        manifest,
        Arc::new(AtomicBool::new(false)),
        Duration::from_secs(5),
    );
    cfg.credential_getter = Some(getter);
    cfg.allow_loopback = true;
    HttpExecutor::new(cfg)
}

#[tokio::test]
async fn bearer_auth_applied_on_base_host() {
    let (listener, base_url) = bind_loopback("127.0.0.1").await;
    let captured = serve_sequence(listener, vec![ok_200()]);

    let manifest = manifest_with_auth(
        &base_url,
        &["127.0.0.1/32"],
        r#"{"scheme":"bearer","cred_ref":"test.cred"}"#,
    );
    let exec = loopback_executor(manifest, bearer_getter());

    exec.call("op", &json!({})).await.expect("call succeeds");

    let req = captured.lock().unwrap()[0].clone();
    assert!(
        req.to_lowercase().contains("authorization: bearer secret123"),
        "bearer header must be present: {req}"
    );
}

#[tokio::test]
async fn header_scheme_applies_the_declared_header_name() {
    let (listener, base_url) = bind_loopback("127.0.0.1").await;
    let captured = serve_sequence(listener, vec![ok_200()]);

    let manifest = manifest_with_auth(
        &base_url,
        &["127.0.0.1/32"],
        r#"{"scheme":"header","cred_ref":"test.cred","header_name":"X-Api-Key"}"#,
    );
    let exec = loopback_executor(manifest, bearer_getter());

    exec.call("op", &json!({})).await.expect("call succeeds");

    let req = captured.lock().unwrap()[0].clone();
    assert!(
        req.to_lowercase().contains("x-api-key: secret123"),
        "custom header must carry the secret: {req}"
    );
}

#[tokio::test]
async fn query_param_scheme_places_the_secret_on_the_request_line() {
    let (listener, base_url) = bind_loopback("127.0.0.1").await;
    let captured = serve_sequence(listener, vec![ok_200()]);

    let manifest = manifest_with_auth(
        &base_url,
        &["127.0.0.1/32"],
        r#"{"scheme":"query-param","cred_ref":"test.cred","param_name":"api_key"}"#,
    );
    let exec = loopback_executor(manifest, bearer_getter());

    exec.call("op", &json!({})).await.expect("call succeeds");

    let req = captured.lock().unwrap()[0].clone();
    assert!(
        req.contains("api_key=SECRET123"),
        "query-param scheme must place the secret in the request line: {req}"
    );
}

#[tokio::test]
async fn same_host_redirect_still_carries_auth() {
    let (listener, base_url) = bind_loopback("127.0.0.1").await;
    // The redirect target is the SAME host (same address:port, different path) — auth must
    // still apply on the second hop.
    let redirect_location = format!("{base_url}/second");
    let captured = serve_sequence(listener, vec![redirect_to(&redirect_location), ok_200()]);

    let manifest = manifest_with_auth(
        &base_url,
        &["127.0.0.1/32"],
        r#"{"scheme":"bearer","cred_ref":"test.cred"}"#,
    );
    let exec = loopback_executor(manifest, bearer_getter());

    exec.call("op", &json!({})).await.expect("call succeeds");

    let reqs = captured.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2, "must have followed the redirect to a second hop");
    for (i, r) in reqs.iter().enumerate() {
        assert!(
            r.to_lowercase().contains("authorization: bearer secret123"),
            "hop {i} on the SAME host must still carry auth: {r}"
        );
    }
}

/// C1 — the headline requirement of this phase: a redirect from the manifest's own host to
/// a DIFFERENT host must NOT carry the secret, even though the SSRF allowance permits
/// reaching it. `127.0.0.1` and `127.0.0.2` are both loopback but are genuinely DIFFERENT
/// host literals by `Url::host_str()` — the exact comparison `HttpExecutor::hop_host_matches`
/// performs in production against a real public attacker host. Modeling it this way lets
/// the test stay fully offline/loopback while still exercising the real host-equality gate,
/// per the phase's own guidance on the loopback SSRF test constraint.
#[tokio::test]
async fn cross_host_redirect_drops_auth_c1() {
    let (listener_b, base_url_b) = bind_loopback("127.0.0.2").await;
    let captured_b = serve_sequence(listener_b, vec![ok_200()]);

    let (listener_a, base_url_a) = bind_loopback("127.0.0.1").await;
    let captured_a = serve_sequence(listener_a, vec![redirect_to(&format!("{base_url_b}/"))]);

    let manifest = manifest_with_auth(
        &base_url_a,
        &["127.0.0.1/32", "127.0.0.2/32"],
        r#"{"scheme":"bearer","cred_ref":"test.cred"}"#,
    );
    let exec = loopback_executor(manifest, bearer_getter());

    exec.call("op", &json!({})).await.expect("call succeeds");

    let req_a = captured_a.lock().unwrap()[0].clone();
    let req_b = captured_b.lock().unwrap()[0].clone();
    assert!(
        req_a.to_lowercase().contains("authorization: bearer secret123"),
        "first hop (the manifest's own host) must carry auth: {req_a}"
    );
    assert!(
        !req_b.to_lowercase().contains("secret123"),
        "cross-host redirect target must NOT receive the secret: {req_b}"
    );
}

/// `Some(auth)` with a `cred_ref` the getter does not recognize fails closed — proven by
/// NOTHING being captured: a request that actually reached the server would have pushed an
/// entry into `captured` before this assertion runs.
#[tokio::test]
async fn unresolvable_credential_fails_closed_no_request_sent() {
    let (listener, base_url) = bind_loopback("127.0.0.1").await;
    let captured = serve_sequence(listener, vec![ok_200()]);

    let manifest = manifest_with_auth(
        &base_url,
        &["127.0.0.1/32"],
        r#"{"scheme":"bearer","cred_ref":"unknown.cred"}"#,
    );
    let exec = loopback_executor(manifest, bearer_getter()); // getter only knows "test.cred"

    let result = exec.call("op", &json!({})).await;
    assert!(result.is_err(), "an unresolvable credential must fail closed");
    // Give the (never-reached) server a moment to prove it received nothing.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        captured.lock().unwrap().is_empty(),
        "no request may leave the executor when the secret is unavailable"
    );
}

/// A manifest with NO `auth` section sends no credential at all (v1 backward compat) even
/// when a getter IS injected — the getter is only ever consulted when `auth` is present.
#[tokio::test]
async fn no_auth_section_sends_no_credential() {
    let (listener, base_url) = bind_loopback("127.0.0.1").await;
    let captured = serve_sequence(listener, vec![ok_200()]);

    let manifest_json = format!(
        r#"{{"connector_name":"test-connector","version":"1","base_url":"{base_url}",
            "allowed_ip_cidrs":["127.0.0.1/32"],"ops":[]}}"#
    );
    let manifest = Arc::new(manifest::parse(&manifest_json).expect("manifest without auth parses"));
    let exec = loopback_executor(manifest, bearer_getter());

    exec.call("op", &json!({})).await.expect("call succeeds");
    let req = captured.lock().unwrap()[0].clone();
    assert!(
        !req.to_lowercase().contains("authorization"),
        "no auth section declared → no Authorization header: {req}"
    );
}

// ---- m1: query-param auth is applied fresh per hop, never inherited from the URL --------

/// m1 (behavioral half): even when a same-host redirect's `Location` echoes back a STALE or
/// attacker-supplied query value (simulating a proxy/compromised host reflecting the
/// incoming querystring), the second hop's actual request still carries the CURRENT secret
/// applied fresh by `apply_auth` via the request builder — never a value inherited from
/// `current_url`. This is what makes the log-safety half of m1 possible: since the secret
/// never gets baked into the URL string the redirect walker carries forward, there is
/// nothing for the diagnostic log to leak in the first place. The log line's own redaction
/// (dropping the query before logging, as defense in depth for a URL that legitimately did
/// carry one) is unit-tested directly against `security::redact_query_for_log` — capturing
/// live `tracing` output here would be racy under `cargo test`'s parallel test-thread
/// execution, since `tracing`'s per-callsite interest cache is process-global and other
/// tests in this file hit the SAME `security.rs` callsite with no subscriber installed.
#[tokio::test]
async fn query_param_auth_is_reapplied_fresh_never_inherited_from_the_redirect_url() {
    let (listener, base_url) = bind_loopback("127.0.0.1").await;
    let redirect_location = format!("{base_url}/next?api_key=STALE_OR_ATTACKER_VALUE");
    let captured = serve_sequence(listener, vec![redirect_to(&redirect_location), ok_200()]);

    let manifest = manifest_with_auth(
        &base_url,
        &["127.0.0.1/32"],
        r#"{"scheme":"query-param","cred_ref":"test.cred","param_name":"api_key"}"#,
    );
    let exec = loopback_executor(manifest, bearer_getter());

    exec.call("op", &json!({})).await.expect("call succeeds");

    let reqs = captured.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2, "must have followed the same-host redirect to a second hop");
    for (i, r) in reqs.iter().enumerate() {
        assert!(
            r.contains("api_key=SECRET123"),
            "hop {i} must carry the FRESH secret applied by apply_auth: {r}"
        );
        assert!(
            !r.contains("STALE_OR_ATTACKER_VALUE"),
            "hop {i} must never carry a query value inherited from the redirect Location: {r}"
        );
    }
}
