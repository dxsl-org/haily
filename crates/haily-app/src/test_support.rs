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
        let tx = self.req_tx.lock().expect("lock").clone().expect("adapter not started");
        tx.send(Request {
            session_id,
            adapter_id: "mock".to_string(),
            message: message.to_string(),
            user_ref: None,
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
        self.delivered.lock().expect("lock").push((session_id, chunk));
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

/// Minimal OpenAI-compatible HTTP/1.1 responder. Reads and discards the request,
/// waits `delay` (simulating LLM latency), then writes a fixed completion. Returns
/// the bound `http://127.0.0.1:PORT` base URL.
pub async fn spawn_slow_llm_server(delay: std::time::Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { break };
            let delay = delay;
            tokio::spawn(async move {
                // Drain the request headers/body; a fixed buffer is enough for the
                // small JSON payloads this test sends.
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf).await;

                tokio::time::sleep(delay).await;

                let body = serde_json::json!({
                    "choices": [{ "message": { "content": "mock completion" } }]
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
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
