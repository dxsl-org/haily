//! Eval-runner unit tests: the SEC-H origin gate, the scoped auto-responder policy, manifest
//! parsing, and report rendering. The end-to-end model-driving path (retry/escalation/pause/
//! depth + ship-block + IrreversibleWrite→deny) is exercised by the scripted-LLM integration
//! goldens in `crates/haily-core/tests/coding_goldens.rs`.

use super::*;
use crate::approval::ApprovalBroker;
use haily_types::{RequestOrigin, ResponseChunk};
use tokio::sync::mpsc;
use uuid::Uuid;

fn chat_request() -> Request {
    Request {
        session_id: Uuid::new_v4(),
        adapter_id: "cli".to_string(),
        message: "please run eval coding".to_string(),
        user_ref: None,
        depth: DepthMode::Normal,
        origin: RequestOrigin::Chat,
    }
}

// ---------------------------------------------------------------------------
// SEC-H: eval mode is structurally unreachable from a chat-origin request.
// ---------------------------------------------------------------------------

#[test]
fn chat_origin_request_can_never_enable_eval_mode() {
    // The default origin for EVERY I/O adapter (GUI, CLI REPL, Telegram) is Chat.
    assert_eq!(RequestOrigin::default(), RequestOrigin::Chat);
    let req = chat_request();
    assert!(
        EvalMode::from_request(&req).is_none(),
        "SEC-H: a chat-origin Request must NEVER enable eval mode"
    );
    assert!(EvalMode::from_origin(RequestOrigin::Chat).is_none());
}

#[test]
fn only_a_cli_origin_request_enables_eval_mode() {
    let mut req = chat_request();
    req.origin = RequestOrigin::Cli;
    assert!(
        EvalMode::from_request(&req).is_some(),
        "a CLI-origin request is the ONLY origin that enables eval mode"
    );
    assert!(EvalMode::from_origin(RequestOrigin::Cli).is_some());
}

#[test]
fn a_wire_deserialized_request_is_always_chat_origin() {
    // origin is #[serde(skip)] — a payload from ANY serialized transport (wire/GUI/persisted),
    // even one that tried to inject "cli", deserializes to the default Chat, so a remote/chat
    // payload can never carry a Cli origin.
    let injected = r#"{"session_id":"00000000-0000-0000-0000-000000000000","adapter_id":"telegram","message":"eval","user_ref":null,"origin":"cli"}"#;
    let req: Request = serde_json::from_str(injected).expect("deserialize");
    assert_eq!(req.origin, RequestOrigin::Chat, "a wire payload can never inject a Cli origin");
    assert!(EvalMode::from_request(&req).is_none());
}

// ---------------------------------------------------------------------------
// Scoped auto-responder: plan checkpoint → approve; any IrreversibleWrite → deny (FMA-M4).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_responder_approves_plan_checkpoint_but_denies_irreversible_write() {
    let broker = Arc::new(ApprovalBroker::new());
    let session_id = Uuid::new_v4();
    let (user_tx, user_rx) = mpsc::channel::<ResponseChunk>(16);
    let responder = spawn_eval_auto_responder(user_rx, Arc::clone(&broker), session_id);

    // A pipeline checkpoint (the plan gate) must be AUTO-APPROVED.
    let plan_id = Uuid::new_v4();
    let cancel = CancellationToken::new();
    let plan_broker = Arc::clone(&broker);
    let plan_decision = {
        let fut = async move { plan_broker.request(plan_id, session_id, &cancel).await };
        let sender = user_tx.clone();
        let (d, _) = tokio::join!(fut, async move {
            tokio::task::yield_now().await;
            sender
                .send(ResponseChunk::ToolApprovalRequest {
                    tool: "pipeline_checkpoint".to_string(),
                    args: "{}".to_string(),
                    approval_id: plan_id,
                    origin: Some("pipeline".to_string()),
                    reversible: false,
                })
                .await
                .unwrap();
        });
        d
    };
    assert!(plan_decision, "the plan checkpoint must be auto-approved");

    // A genuine IrreversibleWrite (e.g. worktree_apply) must be DENIED — never auto-approved.
    let write_id = Uuid::new_v4();
    let cancel2 = CancellationToken::new();
    let write_broker = Arc::clone(&broker);
    let write_decision = {
        let fut = async move { write_broker.request(write_id, session_id, &cancel2).await };
        let sender = user_tx.clone();
        let (d, _) = tokio::join!(fut, async move {
            tokio::task::yield_now().await;
            sender
                .send(ResponseChunk::ToolApprovalRequest {
                    tool: "worktree_apply".to_string(),
                    args: "{}".to_string(),
                    approval_id: write_id,
                    origin: Some("L1:developer".to_string()),
                    reversible: false,
                })
                .await
                .unwrap();
        });
        d
    };
    assert!(!write_decision, "an IrreversibleWrite in eval must be denied → deterministic Failure");

    drop(user_tx);
    let _ = responder.await;
}

// ---------------------------------------------------------------------------
// Manifest parsing (the P9 task.yaml subset).
// ---------------------------------------------------------------------------

#[test]
fn parses_a_full_task_manifest() {
    let src = r#"
# a fixture manifest
id: rust-fix-compile
language: rust
kind: fix-compile-error
description: "Fix the add function so the crate builds and tests pass."
gate: cargo test
max_tool_calls: 25
max_escalations: 0
timeout_seconds: 120
calibration: hard
invariants:
  - "no writes outside the workspace root"
  - "the test module is unchanged"
"#;
    let m = parse_task_yaml(src).expect("parse");
    assert_eq!(m.id, "rust-fix-compile");
    assert_eq!(m.language, "rust");
    assert_eq!(m.kind, "fix-compile-error");
    assert_eq!(m.gate, "cargo test");
    assert_eq!(m.max_tool_calls, 25);
    assert_eq!(m.timeout_seconds, 120);
    assert_eq!(m.calibration.as_deref(), Some("hard"));
    assert_eq!(m.invariants.len(), 2);
    let cmd = m.gate_cmd().unwrap();
    assert_eq!(cmd.program, "cargo");
    assert_eq!(cmd.args, vec!["test".to_string()]);
}

#[test]
fn a_missing_required_field_fails_loud() {
    let src = "id: x\nlanguage: rust\n"; // missing kind/description/gate/...
    assert!(parse_task_yaml(src).is_err(), "a malformed fixture must fail loud, never silently skip");
}

// ---------------------------------------------------------------------------
// Report rendering + egress summary.
// ---------------------------------------------------------------------------

fn sample_outcome(passed: bool) -> EvalOutcome {
    EvalOutcome {
        task_id: "rust-fix-compile".to_string(),
        model: "test-model".to_string(),
        tier_config: "local".to_string(),
        depth: "normal".to_string(),
        score: score(&ScoreInputs {
            gate_exit: Some(if passed { 0 } else { 1 }),
            fixture_original_unchanged: true,
            journal_rows: 2,
            ship_applied: false,
        }),
        escalation_count: 1,
        wall_clock_ms: 4200,
        egress: vec![
            EgressTag { attempt: 0, tier: "fast".to_string(), egress: "local".to_string() },
            EgressTag { attempt: 1, tier: "thinking".to_string(), egress: "cloud".to_string() },
        ],
        per_stage_tokens: Vec::new(),
        eval_run_id: None,
    }
}

#[test]
fn report_renders_verdict_and_gate_table() {
    let out = sample_outcome(true);
    let md = render_outcome(&out);
    assert!(md.contains("Eval: rust-fix-compile"));
    assert!(md.contains("Verdict: PASS"));
    assert!(md.contains("| Gate | Result | Detail |"));
    assert!(md.contains("builds_and_tests_pass"));
    // FMA-M2 egress is surfaced in the report.
    assert!(md.contains("local×1, cloud×1"), "egress summary: {md}");

    let failed = render_outcome(&sample_outcome(false));
    assert!(failed.contains("Verdict: FAIL"));
}

#[test]
fn full_report_wraps_sections() {
    let sections = vec![render_outcome(&sample_outcome(true))];
    let doc = render_report("Coding Eval — 2026", &sections);
    assert!(doc.starts_with("# Coding Eval — 2026"));
    assert!(doc.contains("Eval: rust-fix-compile"));
    assert!(render_report("empty", &[]).contains("No eval tasks"));
}
