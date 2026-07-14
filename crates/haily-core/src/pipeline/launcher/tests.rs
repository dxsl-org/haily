//! Launcher integration tests (Pipeline Activation & Wiring, phase 1): a scripted-LLM Plan run
//! driven through `launch_coding_run` end-to-end — the live `RunEvent` stream a real app-layer
//! bridge would drain, `coding_workspaces.run_id` stamping + retain-on-non-Done, and the real
//! kill switch honored at the next stage boundary.

use super::*;
use crate::approval::ApprovalBroker;
use haily_db::queries::{coding_workspaces, sessions};
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::{LlmConfig, LlmRouter};
use haily_types::{ApprovalResolver, ResponseChunk, RunEvent};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use uuid::Uuid;

const TASK: &str = "add a rate limiter";

async fn git(dir: &std::path::Path, args: &[&str]) {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Fixture {
    db: Arc<DbHandle>,
    kms: Arc<KmsHandle>,
    session_id: Uuid,
    repo: tempfile::TempDir,
    _dbdir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-b", "main"]).await;
    git(repo.path(), &["config", "user.email", "t@haily.test"]).await;
    git(repo.path(), &["config", "user.name", "Test"]).await;
    tokio::fs::write(repo.path().join("README.md"), "hello\n")
        .await
        .unwrap();
    git(repo.path(), &["add", "."]).await;
    git(repo.path(), &["commit", "-m", "init"]).await;

    let dbdir = tempfile::tempdir().unwrap();
    let db = Arc::new(DbHandle::init(&dbdir.path().join("t.db")).await.unwrap());
    let kms = Arc::new(KmsHandle::init((*db).clone(), dbdir.path()).await.unwrap());
    let session_id = Uuid::new_v4();
    sessions::create_session(&db, &session_id.to_string(), "pipeline", None)
        .await
        .unwrap();

    Fixture {
        db,
        kms,
        session_id,
        repo,
        _dbdir: dbdir,
    }
}

/// Scripted OpenAI-compatible responder: `responses[i]` for the i-th completion, then "done".
/// `hits` counts every completion actually consumed, so a test can prove a stage NEVER ran.
async fn spawn_scripted(responses: Vec<String>, hits: Arc<AtomicUsize>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let responses = Arc::new(responses);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let responses = Arc::clone(&responses);
            let hits = Arc::clone(&hits);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let _ = stream.read(&mut buf).await;
                let i = hits.fetch_add(1, Ordering::SeqCst);
                let content = responses
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| "done".to_string());
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

fn emit_valid() -> String {
    r#"<tool_call>{"tool":"emit_plan_draft","args":{"approach":"incremental rollout","rejected":["big bang rewrite"],"phases":[{"phase":1,"title":"Add limiter"}],"assumptions":[{"claim":"api stable","confidence":"high","verification":"cargo check"}]}}</tool_call>"#.to_string()
}
fn render_call() -> String {
    r#"<tool_call>{"tool":"render_plan","args":{}}</tool_call>"#.to_string()
}

/// Auto-approves every pipeline checkpoint reaching `user_rx`.
fn spawn_auto_approver(
    mut user_rx: mpsc::Receiver<ResponseChunk>,
    broker: Arc<ApprovalBroker>,
    session_id: Uuid,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(chunk) = user_rx.recv().await {
            if let ResponseChunk::ToolApprovalRequest { approval_id, .. } = chunk {
                broker.resolve(approval_id, session_id, true);
            }
        }
    })
}

/// Declines exactly the first checkpoint reaching `user_rx`, then keeps draining.
fn spawn_decliner(
    mut user_rx: mpsc::Receiver<ResponseChunk>,
    broker: Arc<ApprovalBroker>,
    session_id: Uuid,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(chunk) = user_rx.recv().await {
            if let ResponseChunk::ToolApprovalRequest { approval_id, .. } = chunk {
                broker.resolve(approval_id, session_id, false);
            }
        }
    })
}

fn deps(
    fx: &Fixture,
    llm: Arc<RwLock<Arc<LlmRouter>>>,
    broker: Arc<ApprovalBroker>,
    kill: Arc<AtomicBool>,
) -> LaunchDeps {
    LaunchDeps {
        db: Arc::clone(&fx.db),
        kms: Arc::clone(&fx.kms),
        llm,
        broker: broker as Arc<dyn haily_types::ApprovalGate>,
        kill,
    }
}

fn plan_spec(fx: &Fixture) -> CodingRunSpec {
    CodingRunSpec {
        kind: RunKind::Plan,
        task: TASK.to_string(),
        session_id: fx.session_id,
        work_item_id: None,
        repo_path: Some(fx.repo.path().to_path_buf()),
        depth: DepthMode::Normal,
    }
}

#[tokio::test]
async fn happy_path_plan_run_completes_and_ships_live_run_events() {
    let fx = fixture().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let llm = build_router(
        spawn_scripted(
            vec![
                "scouted".to_string(),
                emit_valid(),
                "draft recorded".to_string(),
                render_call(),
                "rendered".to_string(),
                "presenting the plan".to_string(),
            ],
            Arc::clone(&hits),
        )
        .await,
    )
    .await;

    let (user_tx, user_rx) = mpsc::channel(64);
    let (ev_tx, mut ev_rx) = mpsc::channel(512);
    let broker = Arc::new(ApprovalBroker::new());
    let responder = spawn_auto_approver(user_rx, Arc::clone(&broker), fx.session_id);

    let report = launch_coding_run(
        deps(
            &fx,
            llm,
            Arc::clone(&broker),
            Arc::new(AtomicBool::new(false)),
        ),
        plan_spec(&fx),
        user_tx,
        ev_tx,
        None,
        CancellationToken::new(),
    )
    .await
    .expect("launch");
    let _ = responder.await;

    assert_eq!(
        report.status,
        RunStatus::Done,
        "the plan launch must complete"
    );

    // The live RunEvent stream a real app-layer bridge (`spawn_run_event_bridge`) would drain —
    // proves the launcher is a genuine producer, not just a wrapper that returns a report.
    let mut saw_started = false;
    let mut saw_complete = false;
    while let Ok(ev) = ev_rx.try_recv() {
        match ev {
            RunEvent::RunStarted { run_id, .. } => {
                assert_eq!(run_id, report.run_id);
                saw_started = true;
            }
            RunEvent::RunComplete { outcome, .. } => {
                assert_eq!(outcome, "done");
                saw_complete = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_started && saw_complete,
        "the launcher must emit a live, ordered RunEvent stream"
    );

    // A `Done` run discards its now-spent workspace (soft-deleted, no longer `list_active`).
    assert!(
        coding_workspaces::list_active(&fx.db)
            .await
            .unwrap()
            .is_empty(),
        "a Done run's ephemeral workspace must be discarded"
    );
}

#[tokio::test]
async fn run_id_is_stamped_and_workspace_retained_on_a_paused_run() {
    let fx = fixture().await;
    let hits = Arc::new(AtomicUsize::new(0));
    // Declined at the approval checkpoint with no revision feedback → `run_plan` leaves the run
    // `Paused` rather than auto-re-running Design.
    let llm = build_router(
        spawn_scripted(
            vec![
                "scouted".to_string(),
                emit_valid(),
                "draft".to_string(),
                render_call(),
                "rendered".to_string(),
                "review".to_string(),
            ],
            Arc::clone(&hits),
        )
        .await,
    )
    .await;

    let (user_tx, user_rx) = mpsc::channel(64);
    let (ev_tx, _ev_rx) = mpsc::channel(512);
    let broker = Arc::new(ApprovalBroker::new());
    let responder = spawn_decliner(user_rx, Arc::clone(&broker), fx.session_id);

    let report = launch_coding_run(
        deps(
            &fx,
            llm,
            Arc::clone(&broker),
            Arc::new(AtomicBool::new(false)),
        ),
        plan_spec(&fx),
        user_tx,
        ev_tx,
        None,
        CancellationToken::new(),
    )
    .await
    .expect("launch");
    let _ = responder.await;

    assert_eq!(
        report.status,
        RunStatus::Paused,
        "a declined plan with no feedback stays paused"
    );

    let rows = coding_workspaces::list_active(&fx.db).await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "a Paused run must retain its workspace for a follow-up trigger"
    );
    assert_eq!(
        rows[0].run_id.as_deref(),
        Some(report.run_id.as_str()),
        "coding_workspaces.run_id must be stamped from the terminal RunReport"
    );
}

#[tokio::test]
async fn kill_switch_aborts_the_run_at_the_next_stage_boundary() {
    let fx = fixture().await;
    let hits = Arc::new(AtomicUsize::new(0));
    // Only the scout stage gets a scripted response — if the kill checkpoint between stages
    // failed to fire, the design stage would consume a SECOND completion (the scripted
    // server's "done" fallback), which the hit-count assertion below would catch.
    let llm =
        build_router(spawn_scripted(vec!["scouted".to_string()], Arc::clone(&hits)).await).await;

    let (user_tx, _user_rx) = mpsc::channel(64);
    let (ev_tx, mut ev_rx) = mpsc::channel(512);
    let broker = Arc::new(ApprovalBroker::new());
    let kill = Arc::new(AtomicBool::new(false));
    let kill_flipper = Arc::clone(&kill);

    // Flip the SAME `Arc<AtomicBool>` handed to `LaunchDeps` as soon as the first stage begins —
    // the runner's kill checkpoint sits BETWEEN stages, so this lands at the design-stage
    // boundary, proving the launcher threads the real kill switch through, not a fresh one.
    let watcher = tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            if let RunEvent::StageStarted { .. } = ev {
                kill_flipper.store(true, Ordering::SeqCst);
                break;
            }
        }
    });

    let report = launch_coding_run(
        deps(&fx, llm, Arc::clone(&broker), Arc::clone(&kill)),
        plan_spec(&fx),
        user_tx,
        ev_tx,
        None,
        CancellationToken::new(),
    )
    .await
    .expect("launch");
    let _ = watcher.await;

    assert_eq!(
        report.status,
        RunStatus::Interrupted,
        "the kill switch must abort the run at the next stage boundary"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "only the scout stage's single scripted response may ever be consumed"
    );
}
