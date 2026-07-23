//! Phase 7 judge machinery tests: panel fan-out (2 lens + 1 synthesis), refuter majority
//! (kills a false positive, survives a genuine finding), and the Ultra-unavailable apex
//! fallback (session tier + warning chunk). Scripted cloud LLM + real DB/KMS — no real model.

use super::*;
use crate::approval::ApprovalBroker;
use haily_db::queries::sessions;
use haily_kms::KmsHandle;
use haily_llm::LlmConfig;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A server that returns `content` verbatim for EVERY request, counting requests. For a
/// no-tool judge sub-turn (lens/synthesis) that means exactly one request per sub-turn.
async fn spawn_constant_server(content: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let count = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&count);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let c = Arc::clone(&c2);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let _ = stream.read(&mut buf).await;
                c.fetch_add(1, Ordering::SeqCst);
                let payload =
                    serde_json::json!({ "choices": [{ "message": { "content": content } }] })
                        .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), count)
}

/// A server that echoes back the request's SYSTEM message as the completion, so a test can
/// assert which lens/synthesis prompt each sub-turn ran under. Counts requests.
async fn spawn_system_echo_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let count = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&count);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let c = Arc::clone(&c2);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let body = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                let sys = serde_json::from_str::<Value>(&req[body..])
                    .ok()
                    .and_then(|v| {
                        v["messages"].as_array().and_then(|m| {
                            m.iter()
                                .find(|x| x["role"] == "system")
                                .and_then(|x| x["content"].as_str().map(str::to_string))
                        })
                    })
                    .unwrap_or_else(|| "no-system".to_string());
                c.fetch_add(1, Ordering::SeqCst);
                let payload = serde_json::json!({ "choices": [{ "message": { "content": sys } }] })
                    .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), count)
}

fn cloud_config(base_url: String) -> LlmConfig {
    LlmConfig {
        cloud_api_keys: vec!["test-key".to_string()],
        cloud_base_url: base_url,
        cloud_model: "test-model".to_string(),
        ..LlmConfig::default()
    }
}

async fn judge_ctx(
    llm: Arc<LlmRouter>,
) -> (
    JudgeContext,
    mpsc::Receiver<ResponseChunk>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(DbHandle::init(&dir.path().join("h.db")).await.expect("db"));
    let kms = Arc::new(
        KmsHandle::init((*db).clone(), dir.path())
            .await
            .expect("kms"),
    );
    let session_id = Uuid::new_v4();
    sessions::create_session(&db, &session_id.to_string(), "judge-test", None)
        .await
        .expect("session");
    let (user_tx, user_rx) = mpsc::channel(64);
    let jc = JudgeContext {
        db,
        kms,
        llm,
        broker: Arc::new(ApprovalBroker::new()),
        kill: Arc::new(AtomicBool::new(false)),
        approval_mode: crate::permission_mode::new_handle(
            crate::permission_mode::ApprovalMode::AcceptEdits,
        ),
        cancel: CancellationToken::new(),
        user_tx,
        session_id,
        turn_deletes: Arc::new(AtomicUsize::new(0)),
    };
    (jc, user_rx, dir)
}

#[tokio::test]
async fn deep_plan_design_runs_two_lenses_and_one_synthesis() {
    let (url, count) = spawn_system_echo_server().await;
    let llm = Arc::new(LlmRouter::init(cloud_config(url)).await);
    let (jc, _rx, _dir) = judge_ctx(llm).await;

    let out = plan_design(&jc, "add a rate limiter to the API", DepthMode::Deep).await;

    assert_eq!(
        out.design_calls, 3,
        "Deep = 2 lens + 1 synthesis design calls"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        3,
        "exactly 3 LLM calls for the Deep panel"
    );
    assert_eq!(out.lens_designs.len(), 2, "both lens designs are retained");
    assert!(
        out.lens_designs[0].contains("RISK-FIRST"),
        "first lens is risk-first"
    );
    assert!(
        out.lens_designs[1].contains("SIMPLICITY-FIRST"),
        "second lens is simplicity-first"
    );
    assert!(
        out.design.contains("synthesizer"),
        "final design came from the grafting synthesis"
    );
}

#[tokio::test]
async fn normal_plan_design_runs_one_design() {
    let (url, count) = spawn_system_echo_server().await;
    let llm = Arc::new(LlmRouter::init(cloud_config(url)).await);
    let (jc, _rx, _dir) = judge_ctx(llm).await;

    let out = plan_design(&jc, "add a rate limiter to the API", DepthMode::Normal).await;

    assert_eq!(
        out.design_calls, 1,
        "Normal = a single design call (cost delta vs Deep=3)"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "exactly 1 LLM call for Normal"
    );
    assert!(out.lens_designs.is_empty(), "Normal runs no lens fan-out");
}

#[tokio::test]
async fn refuter_majority_kills_a_planted_false_positive() {
    // Both refuters confidently refute → majority refute → the finding is killed.
    let (url, _c) = spawn_constant_server(
        r#"<tool_call>{"tool":"emit_refutation","args":{"refuted":true,"reason":"already guarded upstream"}}</tool_call>"#,
    )
    .await;
    let llm = Arc::new(LlmRouter::init(cloud_config(url)).await);
    let (jc, _rx, _dir) = judge_ctx(llm).await;

    let survives = refuter_votes(&jc, "unwrap in prod path", "let x = maybe.unwrap();").await;
    assert!(
        !survives,
        "a finding both refuters confidently refute must be killed"
    );
}

#[tokio::test]
async fn refuter_survives_when_not_refuted() {
    // Neither refuter can refute (a genuine finding) → survives → routes to the fix loop.
    let (url, _c) = spawn_constant_server(
        r#"<tool_call>{"tool":"emit_refutation","args":{"refuted":false,"reason":"real reachable bug"}}</tool_call>"#,
    )
    .await;
    let llm = Arc::new(LlmRouter::init(cloud_config(url)).await);
    let (jc, _rx, _dir) = judge_ctx(llm).await;

    let survives = refuter_votes(
        &jc,
        "SQL injection in query builder",
        "format!(\"...{}\", input)",
    )
    .await;
    assert!(
        survives,
        "a genuine finding no refuter can refute must survive to the fix loop"
    );
}

#[tokio::test]
async fn ultra_unavailable_apex_falls_back_to_session_tier_with_warning() {
    // Local-only config: NO cloud keys → NoopClient primary, no fallback → Ultra unreachable.
    let local_only = LlmConfig {
        cloud_api_keys: vec![],
        ..LlmConfig::default()
    };
    let llm = Arc::new(LlmRouter::init(local_only).await);
    assert!(
        !llm.ultra_reachable(),
        "a keyless local-only router must report Ultra unreachable"
    );
    let (jc, mut rx, _dir) = judge_ctx(llm).await;

    let out = apex_judge(&jc, "A vs B", "some evidence", "pick the safer one").await;
    assert!(
        out.warned_tier_fallback,
        "apex must report it fell back off Ultra"
    );

    // An explicit warning chunk must have reached the user stream.
    let mut warned = false;
    while let Ok(chunk) = rx.try_recv() {
        if let ResponseChunk::Text(t) = chunk {
            if t.contains("Ultra") {
                warned = true;
            }
        }
    }
    assert!(
        warned,
        "an explicit Ultra-unavailable warning chunk must be emitted, not a silent collapse"
    );
}

#[test]
fn refuter_and_verdict_grammars_force_their_synthetic_tools() {
    let rg = tool_grammar(EMIT_REFUTATION_TOOL, &refutation_schema()).expect("refutation grammar");
    assert!(rg.contains("root ::="));
    assert!(rg.contains(EMIT_REFUTATION_TOOL));
    let vg = tool_grammar(EMIT_VERDICT_TOOL, &verdict_schema()).expect("verdict grammar");
    assert!(vg.contains(EMIT_VERDICT_TOOL));
}

#[test]
fn extract_json_value_repairs_fenced_and_prose_wrapped_json() {
    let v = extract_json_value("```json\n{\"refuted\":true}\n``` trailing").expect("repair");
    assert_eq!(v["refuted"], serde_json::json!(true));
    assert!(extract_json_value("no json here").is_none());
}
