//! Integration tests for `security::follow_redirects_with_guard` — proves the
//! manual redirect walker re-vets every hop rather than trusting reqwest's default
//! (10-hop) auto-follow, which would let `302 → http://169.254.169.254/` bypass the
//! SSRF pin entirely (the guard only ever sees the first URL under auto-follow).
//!
//! Uses a raw `TcpListener` fixture server (same pattern as
//! `haily-llm/tests/cloud_stream.rs`) rather than a mocking crate — the fixture
//! bodies are a handful of canned HTTP responses, well within what a small
//! hand-rolled responder can express.
use haily_tools::security::follow_redirects_with_guard;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Starts a one-shot HTTP server that replies with the given raw HTTP response
/// (status line + headers + body) to the first request it receives.
async fn spawn_fixture_server(response: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
        socket.shutdown().await.ok();
    });

    format!("http://{addr}")
}

#[tokio::test]
async fn loopback_fixture_is_itself_blocked_by_the_guard() {
    // The fixture server binds to 127.0.0.1, which `ssrf_guard` correctly blocks as
    // loopback — proving the very first hop (not just redirect targets) is vetted,
    // not only hops reached via a `Location` header. A "clean 200 passthrough"
    // happy-path test would require a real public host, which integration tests
    // must not depend on; `cloud_stream.rs`-style fixtures are loopback-only by
    // construction, so that path is covered instead by the unit-level
    // `ssrf_guard_allows_public_ip_literal_and_pins_addr` test in `security.rs`.
    let body = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\nhello";
    let base_url = spawn_fixture_server(body).await;

    let result =
        follow_redirects_with_guard(&base_url, Duration::from_secs(5), |c, u| c.get(u)).await;
    assert!(
        result.is_err(),
        "loopback must be blocked even with no redirect involved"
    );
}

#[tokio::test]
async fn redirect_to_metadata_ip_is_blocked_not_followed() {
    // 302 pointing at the AWS/GCP/Azure IMDS literal — must be blocked at the
    // `ssrf_guard` re-check on the next hop, never actually connected to.
    let body = "HTTP/1.1 302 Found\r\nlocation: http://169.254.169.254/latest/meta-data/\r\nconnection: close\r\n\r\n";
    let base_url = spawn_fixture_server(body).await;

    let result =
        follow_redirects_with_guard(&base_url, Duration::from_secs(5), |c, u| c.get(u)).await;
    assert!(
        result.is_err(),
        "redirect to cloud metadata IP must be blocked, not followed"
    );
}

#[tokio::test]
async fn redirect_to_ipv6_loopback_is_blocked_not_followed() {
    let body = "HTTP/1.1 302 Found\r\nlocation: http://[::1]/secret\r\nconnection: close\r\n\r\n";
    let base_url = spawn_fixture_server(body).await;

    let result =
        follow_redirects_with_guard(&base_url, Duration::from_secs(5), |c, u| c.get(u)).await;
    assert!(
        result.is_err(),
        "redirect to IPv6 loopback must be blocked, not followed"
    );
}

#[tokio::test]
async fn redirect_to_private_range_is_blocked_not_followed() {
    let body =
        "HTTP/1.1 302 Found\r\nlocation: http://10.0.0.5/internal\r\nconnection: close\r\n\r\n";
    let base_url = spawn_fixture_server(body).await;

    let result =
        follow_redirects_with_guard(&base_url, Duration::from_secs(5), |c, u| c.get(u)).await;
    assert!(
        result.is_err(),
        "redirect to a private-range IP must be blocked, not followed"
    );
}
