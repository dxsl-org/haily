//! Plan Pipeline behavior: stage composition (whitelist + reject-feedback), and scripted-LLM
//! end-to-end runs on the real runner (artifacts on disk, approval blocks until resolved,
//! malformed-draft retry/pause, reject-loop re-runs Design once).

use super::*;
use crate::approval::ApprovalBroker;
use crate::pipeline::runner::PipelineRunner;
use crate::pipeline::{RunSpec, RunStatus};
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::{LlmConfig, LlmRouter};
use haily_tools::coding::workspace::CodingWorkspace;
use haily_tools::ToolRegistry;
use haily_types::{ApprovalResolver, DepthMode, ResponseChunk, RunEvent};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

const SLUG: &str = "251101-plan";
const TASK: &str = "add a rate limiter to the API";

// ---------------------------------------------------------------------------
// Pure composition tests (no runner).
// ---------------------------------------------------------------------------

#[test]
fn scout_stage_is_read_only_and_all_stages_are_leaves() {
    let p = build_plan_pipeline(TASK, SLUG, None, DepthMode::Normal);
    assert_eq!(p.runs.len(), 4, "first pass is scout→design→write→approval");
    let scout = &p.runs[0];
    assert_eq!(scout.name, "scout");
    for read_tool in ["fs_read", "fs_list", "fs_grep"] {
        assert!(scout.tool_whitelist.iter().any(|t| t == read_tool));
    }
    for write_tool in ["fs_write", "fs_edit", "fs_move", "fs_delete"] {
        assert!(
            !scout.tool_whitelist.iter().any(|t| t == write_tool),
            "scout must not be able to call {write_tool}"
        );
    }
    // AD-C1: no stage can delegate.
    assert!(p.all_stages_are_leaves(), "every plan stage must be a leaf");
}

#[test]
fn reject_path_re_runs_design_with_feedback_and_drops_scout() {
    let feedback = "split phase 2 into two smaller phases";
    let p = build_plan_pipeline(TASK, SLUG, Some(feedback), DepthMode::Normal);
    assert_eq!(p.runs.len(), 3, "reject path is design→write→approval (scout dropped)");
    assert_eq!(p.runs[0].name, "design");
    assert!(
        p.runs[0].prompt_ref.contains(feedback),
        "the re-run Design prompt must carry the revision feedback: {}",
        p.runs[0].prompt_ref
    );
}

#[test]
fn design_stage_carries_a_forced_grammar() {
    let p = build_plan_pipeline(TASK, SLUG, None, DepthMode::Normal);
    let design = p.runs.iter().find(|s| s.name == "design").unwrap();
    assert!(design.grammar.is_some(), "the design stage must force the emit_plan_draft grammar");
    assert!(design.grammar.as_deref().unwrap().contains("emit_plan_draft"));
}

// ---------------------------------------------------------------------------
// Fixtures + scripted LLM (mirrors the runner test harness).
// ---------------------------------------------------------------------------

struct Fixture {
    db: Arc<DbHandle>,
    kms: Arc<KmsHandle>,
    session_id: Uuid,
    workspace: CodingWorkspace,
    _dirs: Vec<tempfile::TempDir>,
}

async fn git(dir: &std::path::Path, args: &[&str]) {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

async fn fixture() -> Fixture {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-b", "main"]).await;
    git(repo.path(), &["config", "user.email", "t@haily.test"]).await;
    git(repo.path(), &["config", "user.name", "Test"]).await;
    tokio::fs::write(repo.path().join("README.md"), "hello\n").await.unwrap();
    git(repo.path(), &["add", "."]).await;
    git(repo.path(), &["commit", "-m", "init"]).await;

    let dbdir = tempfile::tempdir().unwrap();
    let db = Arc::new(DbHandle::init(&dbdir.path().join("t.db")).await.unwrap());
    let kms = Arc::new(KmsHandle::init((*db).clone(), dbdir.path()).await.unwrap());
    let session_id = Uuid::new_v4();
    haily_db::queries::sessions::create_session(&db, &session_id.to_string(), "pipeline", None)
        .await
        .unwrap();

    let wt_root = tempfile::tempdir().unwrap();
    let workspace =
        CodingWorkspace::open(&db, &session_id.to_string(), repo.path(), wt_root.path(), None)
            .await
            .expect("open workspace");

    Fixture { db, kms, session_id, workspace, _dirs: vec![repo, dbdir, wt_root] }
}

/// Scripted OpenAI-compatible responder: `responses[i]` for the i-th completion, then "done".
async fn spawn_scripted(responses: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let responses = Arc::new(responses);
    let counter = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { break };
            let responses = Arc::clone(&responses);
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let _ = stream.read(&mut buf).await;
                let i = counter.fetch_add(1, Ordering::SeqCst);
                let content = responses.get(i).cloned().unwrap_or_else(|| "done".to_string());
                let payload =
                    serde_json::json!({ "choices": [{ "message": { "content": content } }] })
                        .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

async fn build_router(base_url: String) -> Arc<RwLock<Arc<LlmRouter>>> {
    let cfg = LlmConfig {
        cloud_api_keys: vec!["test-key".to_string()],
        cloud_base_url: base_url,
        cloud_model: "test-model".to_string(),
        ..LlmConfig::default()
    };
    Arc::new(RwLock::new(Arc::new(LlmRouter::init(cfg).await)))
}

fn plan_tools(workspace: &CodingWorkspace) -> Arc<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    let root = workspace.worktree_root().to_path_buf();
    reg.register(Arc::new(EmitPlanDraftTool::new(root.clone(), SLUG)));
    reg.register(Arc::new(RenderPlanTool::new(root, SLUG, TASK)));
    Arc::new(reg)
}

#[allow(clippy::too_many_arguments)]
fn make_runner(
    fx: &Fixture,
    llm: Arc<RwLock<Arc<LlmRouter>>>,
    base_tools: Arc<ToolRegistry>,
    broker: Arc<dyn haily_types::ApprovalGate>,
    user_tx: tokio::sync::mpsc::Sender<ResponseChunk>,
    events: tokio::sync::mpsc::Sender<RunEvent>,
) -> PipelineRunner {
    PipelineRunner::new(
        Arc::clone(&fx.db),
        Arc::clone(&fx.kms),
        llm,
        base_tools,
        broker,
        Arc::new(AtomicBool::new(false)),
        tokio_util::sync::CancellationToken::new(),
        user_tx,
        events,
        false,
    )
}

fn emit_valid() -> String {
    r#"<tool_call>{"tool":"emit_plan_draft","args":{"approach":"incremental rollout","rejected":["big bang rewrite"],"phases":[{"phase":1,"title":"Add limiter"}],"assumptions":[{"claim":"api stable","confidence":"high","verification":"cargo check"}]}}</tool_call>"#.to_string()
}
fn emit_invalid() -> String {
    r#"<tool_call>{"tool":"emit_plan_draft","args":{"approach":"x","rejected":[],"phases":[]}}</tool_call>"#.to_string()
}
fn render_call() -> String {
    r#"<tool_call>{"tool":"render_plan","args":{}}</tool_call>"#.to_string()
}

/// Spawn a responder that resolves every pipeline approval with `approvals[i]` (defaulting to
/// `true` once exhausted), flagging that at least one approval was seen.
fn spawn_approver(
    mut user_rx: tokio::sync::mpsc::Receiver<ResponseChunk>,
    broker: Arc<ApprovalBroker>,
    session_id: Uuid,
    approvals: Vec<bool>,
    saw: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut i = 0usize;
        // Resolve exactly `approvals.len()` checkpoints then return — the runner keeps a
        // `user_tx` clone alive for its whole lifetime, so looping to channel-close would
        // hang `responder.await`.
        while i < approvals.len() {
            match user_rx.recv().await {
                Some(ResponseChunk::ToolApprovalRequest { approval_id, .. }) => {
                    saw.store(true, Ordering::SeqCst);
                    broker.resolve(approval_id, session_id, approvals[i]);
                    i += 1;
                }
                Some(_) => {}
                None => break,
            }
        }
    })
}

// ---------------------------------------------------------------------------
// End-to-end: task → plan artifacts on disk, approval blocks until resolved.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scripted_plan_run_writes_artifacts_and_blocks_on_approval() {
    let fx = fixture().await;
    let llm = build_router(
        spawn_scripted(vec![
            "scouted".to_string(), // scout
            emit_valid(),          // design tool call
            "draft recorded".to_string(),
            render_call(), // write tool call
            "rendered".to_string(),
            "presenting the plan".to_string(), // approval sub-turn
        ])
        .await,
    )
    .await;

    let (user_tx, user_rx) = tokio::sync::mpsc::channel(64);
    let (ev_tx, _ev_rx) = tokio::sync::mpsc::channel(512);
    let broker = Arc::new(ApprovalBroker::new());
    let saw = Arc::new(AtomicBool::new(false));
    let responder = spawn_approver(user_rx, Arc::clone(&broker), fx.session_id, vec![true], Arc::clone(&saw));

    let runner = make_runner(
        &fx,
        llm,
        plan_tools(&fx.workspace),
        Arc::clone(&broker) as Arc<dyn haily_types::ApprovalGate>,
        user_tx,
        ev_tx,
    );

    let wi = haily_db::queries::work_items::create(&fx.db, &fx.session_id.to_string(), TASK)
        .await
        .unwrap();
    let spec = PlanRunSpec {
        task: TASK.to_string(),
        slug: SLUG.to_string(),
        session_id: fx.session_id,
        work_item_id: Some(wi.id.clone()),
        attempts_budget: 6,
        workspace: &fx.workspace,
        revise_feedback: None,
        depth: DepthMode::Normal,
    };
    let report = run_plan(&runner, &fx.db, spec).await.expect("run");
    let _ = responder.await;

    assert_eq!(report.status, RunStatus::Done, "the plan run must complete");
    assert!(saw.load(Ordering::SeqCst), "the approval checkpoint must have reached the user");

    // Artifacts on disk with the 7-field frontmatter.
    let root = fx.workspace.worktree_root();
    let plan_md = tokio::fs::read_to_string(root.join(".agents").join(SLUG).join("plan.md"))
        .await
        .expect("plan.md exists");
    assert!(plan_md.contains("## Phases"), "plan.md must list phases");
    let phase = tokio::fs::read_to_string(
        root.join(".agents").join(SLUG).join("phase-01-add-limiter.md"),
    )
    .await
    .expect("phase-01 file exists");
    for field in ["phase: 1", "title:", "status:", "priority:", "effort:", "dependencies:", "tier:"] {
        assert!(phase.contains(field), "phase frontmatter missing `{field}`:\n{phase}");
    }

    // Linkage: the work item now points at the rendered plan.
    let linked = haily_db::queries::work_items::get(&fx.db, &wi.id).await.unwrap().unwrap();
    assert_eq!(linked.plan_path.as_deref(), Some(".agents/251101-plan/plan.md"));
}

// ---------------------------------------------------------------------------
// Malformed draft → one retry with parse errors, second failure pauses.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_draft_retries_once_then_pauses() {
    let fx = fixture().await;
    let llm = build_router(
        spawn_scripted(vec![
            "scouted".to_string(),
            emit_invalid(),           // design attempt 1: tool errors, no draft written
            "could not".to_string(),  // design attempt 1 gives up → gate fails
            emit_invalid(),           // design attempt 2 (retry): same
            "could not".to_string(),  // → gate fails again → pause
        ])
        .await,
    )
    .await;

    let (user_tx, _user_rx) = tokio::sync::mpsc::channel(64);
    let (ev_tx, _ev_rx) = tokio::sync::mpsc::channel(512);
    let broker = Arc::new(ApprovalBroker::new());
    let runner = make_runner(
        &fx,
        llm,
        plan_tools(&fx.workspace),
        broker,
        user_tx,
        ev_tx,
    );

    let spec = RunSpec {
        pipeline: build_plan_pipeline(TASK, SLUG, None, DepthMode::Normal),
        session_id: fx.session_id,
        work_item_id: None,
        system_prompt: "test",
        domain_name: "developer",
        attempts_budget: 6,
        workspace: &fx.workspace,
    };
    let report = runner.run(spec).await.expect("run");

    assert_eq!(report.status, RunStatus::Paused, "a twice-malformed draft must pause the run");
    assert_eq!(report.retries, 1, "exactly one design retry before the pause");
    assert!(
        !fx.workspace.worktree_root().join(".agents").join(SLUG).join("plan.md").exists(),
        "no plan.md is rendered when the design stage never produces a valid draft"
    );
}

// ---------------------------------------------------------------------------
// Reject-with-feedback re-runs Design exactly once → Done on the second approval.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn declined_plan_reruns_design_once_and_then_completes() {
    let fx = fixture().await;
    let llm = build_router(
        spawn_scripted(vec![
            // First pass: scout→design→write→approval (declined).
            "scouted".to_string(),
            emit_valid(),
            "draft v1".to_string(),
            render_call(),
            "rendered v1".to_string(),
            "review v1".to_string(),
            // Reject re-run: design→write→approval (approved).
            emit_valid(),
            "draft v2".to_string(),
            render_call(),
            "rendered v2".to_string(),
            "review v2".to_string(),
        ])
        .await,
    )
    .await;

    let (user_tx, user_rx) = tokio::sync::mpsc::channel(64);
    let (ev_tx, _ev_rx) = tokio::sync::mpsc::channel(512);
    let broker = Arc::new(ApprovalBroker::new());
    let saw = Arc::new(AtomicBool::new(false));
    // Decline the first checkpoint, approve the second.
    let responder = spawn_approver(
        user_rx,
        Arc::clone(&broker),
        fx.session_id,
        vec![false, true],
        Arc::clone(&saw),
    );

    let runner = make_runner(
        &fx,
        llm,
        plan_tools(&fx.workspace),
        Arc::clone(&broker) as Arc<dyn haily_types::ApprovalGate>,
        user_tx,
        ev_tx,
    );

    let spec = PlanRunSpec {
        task: TASK.to_string(),
        slug: SLUG.to_string(),
        session_id: fx.session_id,
        work_item_id: None,
        attempts_budget: 6,
        workspace: &fx.workspace,
        revise_feedback: Some("add a phase for load testing".to_string()),
        depth: DepthMode::Normal,
    };
    let report = run_plan(&runner, &fx.db, spec).await.expect("run");
    let _ = responder.await;

    assert_eq!(report.status, RunStatus::Done, "the re-run plan must complete after approval");
    assert!(
        fx.workspace.worktree_root().join(".agents").join(SLUG).join("plan.md").exists(),
        "the re-rendered plan.md must be on disk after the accepted re-run"
    );
}

#[tokio::test]
async fn declined_plan_without_feedback_stays_paused() {
    let fx = fixture().await;
    let llm = build_router(
        spawn_scripted(vec![
            "scouted".to_string(),
            emit_valid(),
            "draft".to_string(),
            render_call(),
            "rendered".to_string(),
            "review".to_string(),
        ])
        .await,
    )
    .await;

    let (user_tx, user_rx) = tokio::sync::mpsc::channel(64);
    let (ev_tx, _ev_rx) = tokio::sync::mpsc::channel(512);
    let broker = Arc::new(ApprovalBroker::new());
    let saw = Arc::new(AtomicBool::new(false));
    let responder =
        spawn_approver(user_rx, Arc::clone(&broker), fx.session_id, vec![false], Arc::clone(&saw));

    let runner = make_runner(
        &fx,
        llm,
        plan_tools(&fx.workspace),
        Arc::clone(&broker) as Arc<dyn haily_types::ApprovalGate>,
        user_tx,
        ev_tx,
    );

    let spec = PlanRunSpec {
        task: TASK.to_string(),
        slug: SLUG.to_string(),
        session_id: fx.session_id,
        work_item_id: None,
        attempts_budget: 6,
        workspace: &fx.workspace,
        revise_feedback: None,
        depth: DepthMode::Normal,
    };
    let report = run_plan(&runner, &fx.db, spec).await.expect("run");
    let _ = responder.await;

    assert_eq!(
        report.status,
        RunStatus::Paused,
        "a declined plan with no revision feedback must stay paused (no auto re-run)"
    );
}
