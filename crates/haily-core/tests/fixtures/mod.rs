//! Shared scaffolding for the golden-task eval harness (`../golden_tasks.rs`).
//!
//! NO NETWORK, NO LLM-AS-JUDGE: `spawn_scripted_sse_server` is a REPLAY-ONLY fixture
//! that speaks the exact SSE wire dialect `haily_llm::cloud`'s `complete_stream`
//! parses (mirrors `haily-core::agent`'s own `turn_integration_tests` mock server,
//! which is the established pattern this harness builds on) — every response is a
//! canned string baked into the test, never a real model call. A local model judging
//! its own output is explicitly disqualified (researcher-03 §5: self-preference bias)
//! and is not used anywhere in this harness; every check below is a deterministic
//! structural/DB assertion.
use haily_core::Orchestrator;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::LlmConfig;
use haily_types::{Request, ResponseChunk};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// One golden task: a fixed input + the scripted LLM outputs that will be replayed,
/// plus deterministic checks against what actually happened.
pub struct GoldenTask {
    pub id: &'static str,
    /// The (VN or EN) user message driving this task — this is the ONLY user-facing
    /// input; everything else is scripted LLM output.
    pub message: &'static str,
    /// SSE delta contents replayed in order, one per LLM completion this turn makes
    /// (a turn with N tool calls needs N+1 entries: N tool-call emissions + 1 final
    /// plain-text answer). The server repeats the LAST entry for any call beyond the
    /// scripted list, so slight under-scripting still yields a deterministic final
    /// answer rather than a hung connection.
    pub scripted_responses: Vec<String>,
    /// The tool name expected to have been dispatched at least once this turn, or
    /// `None` for a plain Q&A task with no tool call.
    pub expected_tool: Option<&'static str>,
    /// The exact `TaskOutcome::as_str()` value expected on this turn's trace.
    pub expected_outcome: &'static str,
}

/// Build a `<tool_call>` SSE delta payload for a scripted response.
pub fn tool_call_content(tool: &str, args: serde_json::Value) -> String {
    format!(r#"<tool_call>{{"tool":"{tool}","args":{args}}}</tool_call>"#)
}

/// Spawn a REPLAY-ONLY SSE responder: `contents[n]` is streamed as a single `data:`
/// delta for the Nth call this server receives (repeating the last entry past the
/// end of the list), then `data: [DONE]`. No network egress — binds to
/// `127.0.0.1:0` (loopback, OS-assigned port), never talks to any real LLM
/// provider. This is the exact mock-server technique `haily-core::agent`'s own
/// `turn_integration_tests` module already uses for `run_turn`'s SSE call path.
pub async fn spawn_scripted_sse_server(contents: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let call_count = Arc::new(AtomicUsize::new(0));
    let contents = Arc::new(contents);

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let call_count = Arc::clone(&call_count);
            let contents = Arc::clone(&contents);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let _ = stream.read(&mut buf).await;

                let n = call_count.fetch_add(1, Ordering::SeqCst);
                let idx = n.min(contents.len().saturating_sub(1));
                let content = contents
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| "Final answer.".to_string());

                let delta = serde_json::json!({
                    "choices": [{ "delta": { "content": content } }]
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

fn cloud_config(base_url: String) -> LlmConfig {
    LlmConfig {
        cloud_api_keys: vec!["test-key".to_string()],
        cloud_base_url: base_url,
        cloud_model: "test-model".to_string(),
        ..LlmConfig::default()
    }
}

/// Fresh DB + KMS in a throwaway temp directory — the same per-test isolation
/// convention `haily-core::agent`'s own test modules use.
async fn fresh_db_kms() -> (Arc<DbHandle>, Arc<KmsHandle>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("haily.db");
    let db = Arc::new(DbHandle::init(&db_path).await.expect("db init"));
    let kms = Arc::new(
        KmsHandle::init((*db).clone(), dir.path())
            .await
            .expect("kms init"),
    );
    (db, kms, dir)
}

/// Outcome of driving one `GoldenTask` end-to-end through the REAL `Orchestrator`
/// (the same public entrypoint `haily-app`'s dispatch loop calls) — collects
/// everything the deterministic checkers in `golden_tasks.rs` need.
pub struct RunOutcome {
    pub session_id: uuid::Uuid,
    pub db: Arc<DbHandle>,
    /// Every `ResponseChunk::Text` fragment concatenated, in arrival order — the
    /// exact bytes a real adapter (GUI/CLI/Telegram) would have rendered to the user.
    pub visible_text: String,
    pub tool_results: Vec<(String, bool)>,
    // Keeps the temp DB/KMS directory alive for the duration of the caller's assertions.
    _dir: tempfile::TempDir,
}

/// Drive one `GoldenTask` through a REAL `Orchestrator::process` call, backed by the
/// replay-only fixture LLM. NO network call happens beyond the loopback mock server
/// spawned above — this is a normal, offline `cargo test` execution path.
pub async fn run_golden_task(task: &GoldenTask) -> RunOutcome {
    let (db, kms, dir) = fresh_db_kms().await;
    let base_url = spawn_scripted_sse_server(task.scripted_responses.clone()).await;

    let shutdown = CancellationToken::new();
    let tasks = TaskTracker::new();
    let orchestrator = Orchestrator::init(
        kms,
        Arc::clone(&db),
        cloud_config(base_url),
        shutdown.clone(),
        tasks.clone(),
        std::collections::HashSet::new(),
        None,
    )
    .await
    .expect("orchestrator init");

    let session_id = uuid::Uuid::new_v4();
    let req = Request {
        session_id,
        adapter_id: "golden-task-harness".to_string(),
        message: task.message.to_string(),
        user_ref: None,
    };

    let (tx, mut rx) = mpsc::channel(64);
    let collector = tokio::spawn(async move {
        let mut visible_text = String::new();
        let mut tool_results = Vec::new();
        while let Some(chunk) = rx.recv().await {
            match chunk {
                ResponseChunk::Text(t) => visible_text.push_str(&t),
                ResponseChunk::ToolResult { name, ok, .. } => tool_results.push((name, ok)),
                _ => {}
            }
        }
        (visible_text, tool_results)
    });

    let cancel = CancellationToken::new();
    orchestrator
        .process(req, tx, cancel)
        .await
        .expect("orchestrator.process");

    let (visible_text, tool_results) = collector.await.expect("collector task");

    // Shut down cleanly so a leaked background worker never bleeds into the next
    // golden task's assertions or timing.
    shutdown.cancel();
    tasks.close();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), tasks.wait()).await;

    RunOutcome {
        session_id,
        db,
        visible_text,
        tool_results,
        _dir: dir,
    }
}

/// Deterministic no-tag-leak checker (reuses the same substring-absence contract
/// `haily-core::agent`'s own streaming tests assert on `ResponseChunk::Text`
/// increments) — case-insensitive since the tag matcher itself tolerates case
/// variants.
pub fn assert_no_tag_leak(visible_text: &str) {
    let lower = visible_text.to_lowercase();
    assert!(
        !lower.contains("<tool_call") && !lower.contains("<tool_result"),
        "tool-protocol tags leaked into user-visible text: {visible_text:?}"
    );
}
