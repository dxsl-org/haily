//! Pairing E2E tests (red team M4, m6) — real HTTP `POST /pair` against the running adapter,
//! not a direct `PairingService` unit call (those already exist in `mobile::pairing::tests`).
use crate::support::{
    connect_ws, hash_token, start_test_server, start_test_server_with_pairing_clock,
};
use haily_io::mobile::pairing::PAIRING_CODE_TTL;
use haily_types::{MobileError, PairRequest, PairResponse};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

async fn pair(port: u16, code: &str, device_name: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/pair"))
        .json(&PairRequest {
            pairing_code: code.to_string(),
            device_name: device_name.to_string(),
        })
        .send()
        .await
        .expect("POST /pair must reach the server")
}

/// The happy path: a pre-approved (headless-style) code redeems to a working device token, and
/// that token immediately authenticates a WS upgrade.
#[tokio::test]
async fn valid_pairing_flow_issues_a_working_device_token() {
    let server = start_test_server(|_| {}).await;
    let code = server.adapter.mint_pairing_code(Some("Phone".into()), true);

    let response = pair(server.port, &code, "Phone").await;
    assert_eq!(response.status(), 200);
    let body: PairResponse = response.json().await.expect("PairResponse body");

    let ws = connect_ws(server.port, &body.device_token).await;
    assert!(
        ws.is_ok(),
        "the freshly-issued token must authenticate the WS upgrade"
    );
}

#[tokio::test]
async fn absent_pairing_code_is_rejected_as_invalid() {
    let server = start_test_server(|_| {}).await;
    let response = pair(server.port, "000000", "Phone").await;
    assert_eq!(response.status(), 404);
    let body: MobileError = response.json().await.expect("MobileError body");
    assert_eq!(body, MobileError::PairingCodeInvalid);
}

/// M4: a code minted but never confirmed (or explicitly denied) must never issue a token.
/// Denying synchronously (rather than waiting out the real 120s confirm-wait timeout) exercises
/// the identical `RedeemOutcome::Denied` → `PairingNotConfirmed` path deterministically.
#[tokio::test]
async fn denied_oob_confirm_pairing_is_rejected() {
    let server = start_test_server(|_| {}).await;
    let code = server
        .adapter
        .mint_pairing_code(Some("Phone".into()), false);
    assert!(server.adapter.confirm_pairing(&code, false));

    let response = pair(server.port, &code, "Phone").await;
    assert_eq!(response.status(), 403);
    let body: MobileError = response.json().await.expect("MobileError body");
    assert_eq!(body, MobileError::PairingNotConfirmed);
}

/// m6: a dropped-ack retry (the phone never saw the first 200 OK and retries with the SAME
/// code) must replay the SAME credentials, never mint a second device row.
#[tokio::test]
async fn dropped_ack_retry_of_an_issued_code_returns_the_same_credentials() {
    let server = start_test_server(|_| {}).await;
    let code = server.adapter.mint_pairing_code(Some("Phone".into()), true);

    let first = pair(server.port, &code, "Phone").await;
    assert_eq!(first.status(), 200);
    let first_body: PairResponse = first.json().await.expect("PairResponse body");

    let retry = pair(server.port, &code, "Phone").await;
    assert_eq!(retry.status(), 200);
    let retry_body: PairResponse = retry.json().await.expect("PairResponse body");

    assert_eq!(first_body.device_id, retry_body.device_id);
    assert_eq!(first_body.device_token, retry_body.device_token);
}

/// Review HIGH (routed from this phase's review, fixed post-review): `docs/mobile-protocol.md`'s
/// pairing sequence diagram states a code must not be "already redeemed for a different device".
/// `PairingService`'s `Issued` state now records the `device_name` from the redeem that produced
/// it and rejects a retry under a DIFFERENT name — closing the window where a captured code
/// (photographed QR, shoulder-surfed `haily pair` code) could ride a legitimate device's
/// already-issued token within the 120s TTL, bypassing M4's out-of-band confirm on the second
/// call. The SAME name still replays identical credentials (m6 dropped-ack idempotency, proven
/// by `dropped_ack_retry_of_an_issued_code_returns_the_same_credentials` above).
#[tokio::test]
async fn redeeming_an_issued_code_under_a_different_device_name_is_rejected_not_replayed() {
    let server = start_test_server(|_| {}).await;
    let code = server
        .adapter
        .mint_pairing_code(Some("Phone A".into()), true);

    let first = pair(server.port, &code, "Phone A").await;
    assert_eq!(first.status(), 200);
    let _first_body: PairResponse = first.json().await.expect("PairResponse body");

    // A DIFFERENT device name on the same still-valid code must be rejected, not replayed.
    let second = pair(server.port, &code, "Someone Else's Phone").await;
    assert_eq!(
        second.status(),
        404,
        "a different device_name on an already-issued code must be rejected as PairingCodeInvalid, \
         never handed the first device's live token"
    );
}

/// Over-rate-limit lockout (per-source-IP, 5/min per `pairing.rs`'s `RATE_LIMIT_PER_MINUTE`).
#[tokio::test]
async fn over_rate_limit_pairing_attempts_are_locked_out() {
    let server = start_test_server(|_| {}).await;
    for _ in 0..5 {
        let response = pair(server.port, "000000", "Phone").await;
        // All 5 are within the limit — each is simply "invalid code" (404), never rate-limited.
        assert_eq!(response.status(), 404);
    }
    let sixth = pair(server.port, "000000", "Phone").await;
    assert_eq!(sixth.status(), 429);
    let body: MobileError = sixth.json().await.expect("MobileError body");
    assert_eq!(body, MobileError::PairingRateLimited);
}

// A dedicated virtual clock, used by exactly ONE test below — no other test in this binary
// reads or advances it, so running the full suite's tests in parallel (cargo's default) can
// never let this shared static leak into an unrelated test's timing.
static CLOCK_BASE: OnceLock<Instant> = OnceLock::new();
static CLOCK_OFFSET_SECS: AtomicU64 = AtomicU64::new(0);

fn virtual_now() -> Instant {
    *CLOCK_BASE.get_or_init(Instant::now)
        + Duration::from_secs(CLOCK_OFFSET_SECS.load(Ordering::SeqCst))
}

/// The full HTTP-level expiry contract (410 Gone + `PairingCodeExpired` JSON body) — distinct
/// coverage from `mobile::pairing::tests::expired_code_is_reaped_and_reported_expired`, which
/// only proves the Rust-level `RedeemOutcome` enum, not the wire status/body.
#[tokio::test]
async fn expired_pairing_code_is_rejected_over_http() {
    CLOCK_OFFSET_SECS.store(0, Ordering::SeqCst);
    let server = start_test_server_with_pairing_clock(|_| {}, virtual_now).await;
    let code = server.adapter.mint_pairing_code(Some("Phone".into()), true);

    CLOCK_OFFSET_SECS.store(PAIRING_CODE_TTL.as_secs() + 1, Ordering::SeqCst);

    let response = pair(server.port, &code, "Phone").await;
    assert_eq!(response.status(), 410);
    let body: MobileError = response.json().await.expect("MobileError body");
    assert_eq!(body, MobileError::PairingCodeExpired);
}

/// Sanity: a manually-registered (bypassing HTTP pairing) token still authenticates — proves
/// the harness's `hash_token`/`FakeDeviceStore::register` seam matches the server's own hashing,
/// which every OTHER test file in this suite relies on.
#[tokio::test]
async fn a_directly_registered_token_authenticates_the_ws_upgrade() {
    let server = start_test_server(|_| {}).await;
    let token = "a-directly-registered-token";
    server.devices.register(&hash_token(token));

    let mut ws = connect_ws(server.port, token)
        .await
        .expect("upgrade must succeed");
    crate::support::send_hello(&mut ws, None, None).await;
    let frame = crate::support::recv_frame_timeout(&mut ws, crate::support::DEFAULT_TIMEOUT)
        .await
        .expect("HelloAck must arrive");
    assert!(matches!(
        frame.body,
        haily_types::ServerBody::HelloAck { .. }
    ));
}
