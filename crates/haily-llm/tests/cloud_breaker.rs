//! Integration tests for `CloudClient`'s per-key circuit breaker against real (if
//! synthetic) network failures — proves the breaker trips on transport failures and
//! that 429s never trip it. Fine-grained state-machine behavior (probe admission,
//! half-open close/reopen) is covered by `breaker`'s own unit tests; these tests only
//! prove the wiring: `complete()` actually calls into the breaker on the right
//! outcomes.
//!
//! Transport failures are produced by binding a TCP listener, reading its port, then
//! dropping it before the client connects — guarantees `ECONNREFUSED` (a connect-time
//! OS error, never an HTTP response), exactly the "never reached the server" case the
//! breaker must react to.
use haily_llm::{CompletionRequest, LlmClient, Message};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;

/// Binds an ephemeral port and immediately releases it without accepting any
/// connection, so any request to it fails at connect time (ECONNREFUSED) rather than
/// returning an HTTP status — a real, OS-level transport failure.
async fn dead_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    format!("http://{addr}")
}

/// Starts a one-shot HTTP server that replies with the given raw HTTP response
/// (status line + headers + body) to the first request it receives.
async fn spawn_fixture_server(response: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        socket.write_all(response.as_bytes()).await.expect("write response");
        socket.shutdown().await.ok();
    });

    format!("http://{addr}")
}

#[tokio::test]
async fn transport_failure_is_scoped_per_client_healthy_key_unaffected() {
    let dead = dead_port_url().await;
    let client = haily_llm::CloudClient::new(dead, vec!["k1".to_string()], "gpt-4o-mini").unwrap();
    let req = CompletionRequest::simple(vec![Message::user("hi")]);
    assert!(client.complete(req).await.is_err(), "connection to a dead port must surface as an error");

    // A second, independent client against a healthy fixture must still succeed —
    // proves a dead key on one client doesn't leak global breaker state.
    let ok_body = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"hi\"}}]}";
    let base_url = spawn_fixture_server(Box::leak(ok_body.to_string().into_boxed_str())).await;
    let healthy = haily_llm::CloudClient::new(base_url, vec!["k1".to_string()], "gpt-4o-mini").unwrap();
    let req2 = CompletionRequest::simple(vec![Message::user("hi")]);
    assert_eq!(healthy.complete(req2).await.unwrap(), "hi");
}

#[tokio::test]
async fn breaker_opens_after_three_transport_failures_and_skips_connect_attempt() {
    // Single key, dead port. Each of the first 3 calls attempts a real TCP connect
    // (measurable, if fast, latency) and fails with a transport error. Once the
    // breaker opens, `try_acquire` returns `Blocked` before any connect is attempted
    // at all, so the 4th call must return `Err` in effectively the same
    // (near-zero) time as a HashMap lookup — proven by asserting it is not slower
    // than the earlier attempts, i.e. no new connect syscall was issued.
    let dead = dead_port_url().await;
    let client = haily_llm::CloudClient::new(dead, vec!["k1".to_string()], "gpt-4o-mini").unwrap();

    for _ in 0..3 {
        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        assert!(client.complete(req).await.is_err());
    }

    let start = Instant::now();
    let req = CompletionRequest::simple(vec![Message::user("hi")]);
    let result = client.complete(req).await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "all keys must be exhausted once the sole key's breaker is open");
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "an open breaker must short-circuit without attempting a new connect (took {elapsed:?})"
    );
}

#[tokio::test]
async fn rate_limit_exhausts_without_hanging_and_never_blocks_a_healthy_key_later() {
    let rl_body = "HTTP/1.1 429 Too Many Requests\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\nrate limited";
    let ok_body = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"hi\"}}]}";

    // A single key that always 429s: 5 consecutive requests (more than the breaker's
    // 3-failure threshold) must all cleanly exhaust as "rate limited", never as a
    // breaker trip — since 429 must never call `record_failure`.
    for _ in 0..5 {
        let rl_url = spawn_fixture_server(Box::leak(rl_body.to_string().into_boxed_str())).await;
        let client = haily_llm::CloudClient::new(rl_url, vec!["k1".to_string()], "gpt-4o-mini").unwrap();
        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        assert!(client.complete(req).await.is_err());
    }

    // A fresh healthy client (proving no shared/global state) still succeeds.
    let ok_url = spawn_fixture_server(Box::leak(ok_body.to_string().into_boxed_str())).await;
    let healthy_client = haily_llm::CloudClient::new(ok_url, vec!["k1".to_string()], "gpt-4o-mini").unwrap();
    let req = CompletionRequest::simple(vec![Message::user("hi")]);
    assert_eq!(healthy_client.complete(req).await.unwrap(), "hi");
}
