//! Shared test doubles for bootstrap/shutdown integration tests (`tests.rs`).
//!
//! Uses a real (tempdir) SQLite DB per the workspace convention (`code-standards.md`
//! — "Database operations — test with real SQLite") and a minimal hand-rolled
//! HTTP/1.1 responder (no new dependency) to give the cloud LLM path a controllable,
//! real network round trip instead of mocking `LlmClient` directly — this is the only
//! way to make a turn genuinely slow enough to prove the shutdown drain waits on it.
use anyhow::Result;
use async_trait::async_trait;
use haily_io::{Adapter, ApprovalResolver, Notification, Request, RequestSender, ResponseChunk};
use haily_llm::LlmConfig;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Test adapter driven entirely by the test body: `send()` injects a `Request`,
/// `chunks_for()` returns every chunk delivered so far for a session.
pub struct MockAdapter {
    req_tx: Mutex<Option<mpsc::Sender<Request>>>,
    delivered: Arc<Mutex<Vec<(Uuid, ResponseChunk)>>>,
    /// Set by `set_approval_resolver` — lets bootstrap tests confirm
    /// `haily-app::bootstrap` actually injects the resolver into every adapter,
    /// not just the ones with a real interactive approval surface.
    approval_resolver: Mutex<Option<Arc<dyn ApprovalResolver>>>,
}

impl MockAdapter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            req_tx: Mutex::new(None),
            delivered: Arc::new(Mutex::new(Vec::new())),
            approval_resolver: Mutex::new(None),
        })
    }

    /// Whether `set_approval_resolver` has been called — proves bootstrap wired the
    /// broker into this adapter before `start()` began accepting requests.
    pub fn has_approval_resolver(&self) -> bool {
        self.approval_resolver.lock().expect("lock").is_some()
    }

    pub async fn send(&self, message: &str) -> Uuid {
        let session_id = Uuid::new_v4();
        let tx = self
            .req_tx
            .lock()
            .expect("lock")
            .clone()
            .expect("adapter not started");
        tx.send(Request {
            session_id,
            adapter_id: "mock".to_string(),
            message: message.to_string(),
            user_ref: None,
            depth: Default::default(),
            origin: Default::default(),
        })
        .await
        .expect("dispatch loop closed");
        session_id
    }

    pub fn chunks_for(&self, session_id: Uuid) -> Vec<ResponseChunk> {
        self.delivered
            .lock()
            .expect("lock")
            .iter()
            .filter(|(id, _)| *id == session_id)
            .map(|(_, c)| c.clone())
            .collect()
    }
}

#[async_trait]
impl Adapter for MockAdapter {
    async fn start(&self, tx: RequestSender) -> Result<()> {
        *self.req_tx.lock().expect("lock") = Some(tx);
        Ok(())
    }

    async fn deliver(&self, session_id: Uuid, chunk: ResponseChunk) -> Result<()> {
        self.delivered
            .lock()
            .expect("lock")
            .push((session_id, chunk));
        Ok(())
    }

    async fn notify(&self, _msg: Notification) -> Result<()> {
        Ok(())
    }

    fn set_approval_resolver(&self, resolver: Arc<dyn ApprovalResolver>) {
        *self.approval_resolver.lock().expect("lock") = Some(resolver);
    }

    fn id(&self) -> &str {
        "mock"
    }
}

/// Minimal OpenAI-compatible SSE responder. Reads and discards the request, waits
/// `delay` (simulating LLM latency), then streams a fixed completion as `data:`
/// events terminated by `[DONE]`. `run_turn` (phase-06 streaming) always requests
/// `"stream": true`, so the mock speaks the same SSE dialect `cloud.rs` actually
/// parses — a non-streaming JSON blob response here would silently pass a
/// pre-streaming test double against post-streaming production code. Returns the
/// bound `http://127.0.0.1:PORT` base URL.
pub async fn spawn_slow_llm_server(delay: std::time::Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let delay = delay;
            tokio::spawn(async move {
                // Drain the request headers/body; a fixed buffer is enough for the
                // small JSON payloads this test sends.
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf).await;

                tokio::time::sleep(delay).await;

                let delta = serde_json::json!({
                    "choices": [{ "delta": { "content": "mock completion" } }]
                })
                .to_string();
                let sse_body = format!("data: {delta}\n\ndata: [DONE]\n\n");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse_body}"
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    format!("http://{addr}")
}

/// SSE responder that drips `token_count` separate `data:` events, sleeping
/// `inter_token_delay` between each — unlike `spawn_slow_llm_server` (whose delay is
/// entirely BEFORE the response starts), this lets a test fire cancellation
/// mid-stream, after some tokens have already arrived but before `[DONE]`. Returns
/// the bound base URL.
pub async fn spawn_streaming_llm_server(
    token_count: u32,
    inter_token_delay: std::time::Duration,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf).await;

                let status_and_headers =
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                if stream
                    .write_all(status_and_headers.as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }

                for i in 0..token_count {
                    tokio::time::sleep(inter_token_delay).await;
                    let delta = serde_json::json!({
                        "choices": [{ "delta": { "content": format!("tok{i} ") } }]
                    })
                    .to_string();
                    let frame = format!("data: {delta}\n\n");
                    if stream.write_all(frame.as_bytes()).await.is_err() {
                        // Client dropped the connection (e.g. cancelled mid-stream) —
                        // stop writing rather than erroring; this mirrors a real
                        // network disconnect from the client side.
                        return;
                    }
                }
                let _ = stream.write_all(b"data: [DONE]\n\n").await;
                let _ = stream.shutdown().await;
            });
        }
    });

    format!("http://{addr}")
}

pub fn cloud_config(base_url: String) -> LlmConfig {
    LlmConfig {
        cloud_api_keys: vec!["test-key".to_string()],
        cloud_base_url: base_url,
        cloud_model: "test-model".to_string(),
        ..LlmConfig::default()
    }
}
