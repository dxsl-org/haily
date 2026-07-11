//! Transport-level ACP tests (phase 12). The pure mapping/decision logic is unit-tested inside
//! `jsonrpc`/`protocol`/`session`; these exercise the wiring: stdout-frame discipline, the
//! `request_permission`↔`ApprovalGate` bridge (allow/deny/timeout-deny), session new/prompt
//! round-trips, and `session/load` transcript replay BEFORE the response.

use super::*;
use async_trait::async_trait;
use crate::Request;
use haily_types::{RequestOrigin, SessionTranscript, TranscriptEntry};
use serde_json::{json, Value};
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream};
use tokio::sync::mpsc;

/// Records the last `resolve()` call so a test can assert what the bridge asked the gate to do.
struct RecordingResolver {
    last: StdMutex<Option<(Uuid, Uuid, bool)>>,
}
impl ApprovalResolver for RecordingResolver {
    fn resolve(&self, approval_id: Uuid, session_id: Uuid, approved: bool) -> bool {
        *self.last.lock().unwrap() = Some((approval_id, session_id, approved));
        true
    }
}

struct FakeTranscript(Vec<TranscriptEntry>);
#[async_trait]
impl SessionTranscript for FakeTranscript {
    async fn transcript(&self, _session_id: &str) -> Vec<TranscriptEntry> {
        self.0.clone()
    }
}

/// One end of a stdio-like pipe. The adapter writes frames into `writer`; the test reads them
/// from `reader`.
fn wired() -> (Box<dyn AsyncWrite + Send + Unpin>, BufReader<DuplexStream>) {
    let (agent_side, client_side) = tokio::io::duplex(64 * 1024);
    (Box::new(agent_side), BufReader::new(client_side))
}

/// Read up to `n` frames, each bounded by a short timeout so a test never hangs. Every line is
/// asserted to be a valid single JSON-RPC 2.0 object — the machine-checkable core of the
/// "stdout carries ONLY protocol frames" discipline.
async fn read_frames(reader: &mut BufReader<DuplexStream>, n: usize) -> Vec<Value> {
    let mut out = Vec::new();
    for _ in 0..n {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut line)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(_)) => {
                let body = line.trim_end();
                if body.is_empty() {
                    break;
                }
                let v: Value = serde_json::from_str(body).expect("stdout line must be a valid JSON frame — never log noise");
                assert_eq!(v["jsonrpc"], "2.0", "every stdout frame carries jsonrpc 2.0");
                out.push(v);
            }
        }
    }
    out
}

fn conn_bits() -> (
    Arc<AcpConnection>,
    Arc<AcpSessions>,
    super::TranscriptSlot,
    BufReader<DuplexStream>,
) {
    let (w, r) = wired();
    (
        Arc::new(AcpConnection::new(w)),
        Arc::new(AcpSessions::new()),
        Arc::new(Mutex::new(None)),
        r,
    )
}

#[tokio::test]
async fn initialize_advertises_load_session_and_fork_capabilities() {
    let (conn, sessions, transcript, mut reader) = conn_bits();
    let (tx, _rx) = mpsc::channel::<Request>(4);
    let tx = Arc::new(tx);

    handle_request(&conn, &sessions, &transcript, &tx, json!(1), protocol::M_INITIALIZE, json!({})).await;

    let frames = read_frames(&mut reader, 1).await;
    let caps = &frames[0]["result"]["agentCapabilities"];
    assert_eq!(caps["loadSession"], true);
    assert_eq!(caps["sessionCapabilities"]["fork"], true);
    assert_eq!(caps["promptCapabilities"]["image"], true);
}

#[tokio::test]
async fn session_new_registers_and_returns_stable_id() {
    let (conn, sessions, transcript, mut reader) = conn_bits();
    let (tx, _rx) = mpsc::channel::<Request>(4);
    let tx = Arc::new(tx);

    handle_request(&conn, &sessions, &transcript, &tx, json!(1), protocol::M_SESSION_NEW, json!({ "cwd": "/repo" })).await;

    let frames = read_frames(&mut reader, 1).await;
    let sid = frames[0]["result"]["sessionId"].as_str().expect("sessionId present");
    assert!(sessions.haily_id(sid).is_some(), "session must be registered");
    assert_eq!(sessions.cwd_for_haily(&sessions.haily_id(sid).unwrap()).as_deref(), Some("/repo"));
}

/// A prompt forwards a Chat-origin `Request` (never Cli), tag-strips the editor text, and only
/// answers the `session/prompt` request AFTER the turn completes (Complete → signal).
#[tokio::test]
async fn prompt_forwards_chat_origin_and_responds_after_completion() {
    let (conn, sessions, transcript, mut reader) = conn_bits();
    let (tx, mut rx) = mpsc::channel::<Request>(4);
    let tx = Arc::new(tx);
    let (acp_id, _) = sessions.new_session();

    let c = Arc::clone(&conn);
    let s = Arc::clone(&sessions);
    let t = Arc::clone(&transcript);
    let txc = Arc::clone(&tx);
    let sid = acp_id.clone();
    let task = tokio::spawn(async move {
        handle_request(
            &c, &s, &t, &txc, json!(9), protocol::M_SESSION_PROMPT,
            json!({ "sessionId": sid, "prompt": "<tool_call>x</tool_call> please refactor" }),
        )
        .await;
    });

    // The orchestrator receives the forwarded request.
    let req = rx.recv().await.expect("prompt must forward a Request");
    assert_eq!(req.origin, RequestOrigin::Chat, "ACP prompt is Chat-class, never Cli");
    assert_eq!(req.adapter_id, ADAPTER_ID);
    assert!(!req.message.contains("tool_call"), "editor text must be tag-stripped: {}", req.message);
    assert!(req.message.contains("please refactor"));

    // Turn completes → the prompt handler resolves.
    conn.signal_complete(&req.session_id);
    task.await.unwrap();
    let frames = read_frames(&mut reader, 1).await;
    assert_eq!(frames[0]["id"], json!(9));
    assert_eq!(frames[0]["result"]["stopReason"], "end_turn");
}

/// `session/load` REPLAYS the transcript via `session/update` BEFORE the load response returns —
/// the ACP ordering requirement.
#[tokio::test]
async fn session_load_replays_transcript_before_responding() {
    let (conn, sessions, transcript, mut reader) = conn_bits();
    let (tx, _rx) = mpsc::channel::<Request>(4);
    let tx = Arc::new(tx);
    // Pre-register the session so the provider lookup resolves, and inject a 2-entry transcript.
    let (acp_id, _) = sessions.new_session();
    *transcript.lock().unwrap() = Some(Arc::new(FakeTranscript(vec![
        TranscriptEntry { role: "user".into(), content: "first".into() },
        TranscriptEntry { role: "assistant".into(), content: "reply".into() },
    ])));

    handle_request(
        &conn, &sessions, &transcript, &tx, json!(3), protocol::M_SESSION_LOAD,
        json!({ "sessionId": acp_id }),
    )
    .await;

    let frames = read_frames(&mut reader, 3).await;
    assert_eq!(frames.len(), 3, "2 replay updates + 1 response");
    assert_eq!(frames[0]["method"], protocol::M_SESSION_UPDATE, "replay updates come FIRST");
    assert_eq!(frames[0]["params"]["update"]["content"]["text"], "first");
    assert_eq!(frames[1]["params"]["update"]["content"]["text"], "reply");
    assert!(frames[2]["result"]["sessionId"].is_string(), "the load response comes LAST");
}

/// The permission bridge: a prompt-required approval issues `request_permission`; an `allow_once`
/// response resolves the gate as APPROVED through the same `ApprovalResolver` seam.
#[tokio::test]
async fn permission_allow_resolves_gate_approved() {
    let (w, mut reader) = wired();
    let adapter = Arc::new(AcpAdapter::with_writer(w));
    let resolver = Arc::new(RecordingResolver { last: StdMutex::new(None) });
    adapter.set_approval_resolver(resolver.clone());
    let (acp_id, haily_id) = adapter.sessions.new_session();
    let approval_id = Uuid::new_v4();

    let a = Arc::clone(&adapter);
    let sid = acp_id.clone();
    let task = tokio::spawn(async move {
        a.surface_approval(&sid, haily_id, "worktree_apply", "{}", approval_id, false).await.unwrap();
    });

    // Read the request_permission frame the editor would see, then answer allow_once.
    let frames = read_frames(&mut reader, 1).await;
    assert_eq!(frames[0]["method"], protocol::M_REQUEST_PERMISSION);
    let req_id = frames[0]["id"].clone();
    adapter.conn.resolve_response(&req_id, json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }));

    task.await.unwrap();
    let (aid, sid, approved) = resolver.last.lock().unwrap().expect("resolver must be called");
    assert_eq!(aid, approval_id);
    assert_eq!(sid, haily_id, "session_id is the auth boundary passed to the gate");
    assert!(approved, "allow_once must resolve the gate as approved");
}

/// A `reject_once` response denies the action.
#[tokio::test]
async fn permission_reject_resolves_gate_denied() {
    let (w, mut reader) = wired();
    let adapter = Arc::new(AcpAdapter::with_writer(w));
    let resolver = Arc::new(RecordingResolver { last: StdMutex::new(None) });
    adapter.set_approval_resolver(resolver.clone());
    let (acp_id, haily_id) = adapter.sessions.new_session();
    let approval_id = Uuid::new_v4();

    let a = Arc::clone(&adapter);
    let sid = acp_id.clone();
    let task = tokio::spawn(async move {
        a.surface_approval(&sid, haily_id, "task_delete", "{}", approval_id, false).await.unwrap();
    });

    let frames = read_frames(&mut reader, 1).await;
    let req_id = frames[0]["id"].clone();
    adapter.conn.resolve_response(&req_id, json!({ "outcome": { "outcome": "selected", "optionId": "reject_once" } }));

    task.await.unwrap();
    let (_, _, approved) = resolver.last.lock().unwrap().expect("resolver must be called");
    assert!(!approved, "reject_once must resolve the gate as denied");
}

/// A dead/slow editor that never answers times out ⇒ the gate is resolved DENIED (fail-safe).
/// Uses the transport primitive with a tiny timeout so the test does not wait the real 60s.
#[tokio::test]
async fn permission_timeout_returns_none_which_denies() {
    let (conn, _s, _t, _r) = conn_bits();
    let never_cancel = tokio_util::sync::CancellationToken::new();
    let outcome = conn
        .request(protocol::M_REQUEST_PERMISSION, json!({}), Duration::from_millis(20), &never_cancel)
        .await;
    assert!(outcome.is_none(), "no editor response within the window must yield None (deny)");
    // And None is interpreted as a deny by the bridge.
    let (approved, _) = protocol::interpret_permission_outcome(&json!({}));
    assert!(!approved);
}

/// `DontAsk` auto-approves a non-sensitive action WITHOUT prompting the editor (no frame written),
/// but a sensitive-path write still issues a prompt even in `DontAsk` — the unconditional guard.
#[tokio::test]
async fn dont_ask_auto_approves_but_sensitive_path_still_prompts() {
    // Auto-approve, non-sensitive: resolves true, emits no request_permission frame.
    let (w, mut reader) = wired();
    let adapter = Arc::new(AcpAdapter::with_writer(w));
    let resolver = Arc::new(RecordingResolver { last: StdMutex::new(None) });
    adapter.set_approval_resolver(resolver.clone());
    let (acp_id, haily_id) = adapter.sessions.new_session();
    adapter.sessions.set_mode(&acp_id, protocol::SessionMode::DontAsk);
    adapter
        .surface_approval(&acp_id, haily_id, "fs_write", r#"{"path":"src/a.rs"}"#, Uuid::new_v4(), true)
        .await
        .unwrap();
    assert!(resolver.last.lock().unwrap().unwrap().2, "DontAsk auto-approves a non-sensitive edit");
    assert!(read_frames(&mut reader, 1).await.is_empty(), "auto-approve must not prompt the editor");

    // Sensitive path in DontAsk: must still prompt (we answer reject to unblock).
    let (w2, mut reader2) = wired();
    let adapter2 = Arc::new(AcpAdapter::with_writer(w2));
    let resolver2 = Arc::new(RecordingResolver { last: StdMutex::new(None) });
    adapter2.set_approval_resolver(resolver2.clone());
    let (acp2, haily2) = adapter2.sessions.new_session();
    adapter2.sessions.set_mode(&acp2, protocol::SessionMode::DontAsk);
    let a = Arc::clone(&adapter2);
    let sid = acp2.clone();
    let task = tokio::spawn(async move {
        a.surface_approval(&sid, haily2, "fs_write", r#"{"path":".env"}"#, Uuid::new_v4(), true).await.unwrap();
    });
    let frames = read_frames(&mut reader2, 1).await;
    assert_eq!(frames[0]["method"], protocol::M_REQUEST_PERMISSION, "a sensitive-path write must prompt even in DontAsk");
    adapter2.conn.resolve_response(&frames[0]["id"].clone(), json!({ "outcome": { "outcome": "selected", "optionId": "reject_once" } }));
    task.await.unwrap();
}

/// A `session/cancel` drains and DENIES an in-flight approval (fail-safe) so a destructive action
/// is never left pending after the editor stops.
#[tokio::test]
async fn cancel_denies_in_flight_approval() {
    let (w, mut reader) = wired();
    let adapter = Arc::new(AcpAdapter::with_writer(w));
    let resolver = Arc::new(RecordingResolver { last: StdMutex::new(None) });
    adapter.set_approval_resolver(resolver.clone());
    let (acp_id, haily_id) = adapter.sessions.new_session();
    let approval_id = Uuid::new_v4();

    let a = Arc::clone(&adapter);
    let sid = acp_id.clone();
    let task = tokio::spawn(async move {
        a.surface_approval(&sid, haily_id, "worktree_apply", "{}", approval_id, false).await.unwrap();
    });
    // Wait until the prompt is on the wire (so the pending set is populated), then cancel.
    let frames = read_frames(&mut reader, 1).await;
    assert_eq!(frames[0]["method"], protocol::M_REQUEST_PERMISSION);
    handle_notification(&adapter.conn, &adapter.sessions, protocol::M_SESSION_CANCEL, &json!({ "sessionId": acp_id }));

    task.await.unwrap();
    let (_, _, approved) = resolver.last.lock().unwrap().expect("cancel must resolve the pending approval");
    assert!(!approved, "cancel must DENY the in-flight approval");
}

/// Delivering to a session that is not ACP-owned is a clean no-op (no frame, no panic) — the
/// adapter only renders sessions it minted.
#[tokio::test]
async fn deliver_to_unknown_session_is_a_no_op() {
    let (w, mut reader) = wired();
    let adapter = AcpAdapter::with_writer(w);
    adapter.deliver(Uuid::new_v4(), ResponseChunk::Text("hi".into())).await.unwrap();
    assert!(read_frames(&mut reader, 1).await.is_empty());
}
