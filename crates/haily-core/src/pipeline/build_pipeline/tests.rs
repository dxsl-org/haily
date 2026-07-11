//! Build Pipeline behavior: stage composition (whitelist, distinct reviewer prompt, scaffold
//! tier gating, single write-path) and scripted-LLM end-to-end runs on the real runner —
//! clean-review ship, planted-Critical fix loop, and unresolved-Critical pause (nothing shipped).

use super::*;
use crate::approval::ApprovalBroker;
use crate::pipeline::runner::PipelineRunner;
use crate::pipeline::RunStatus;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::{LlmConfig, LlmRouter, Tier};
use haily_tools::coding::workspace::CodingWorkspace;
use haily_tools::ToolRegistry;
use haily_types::{ApprovalResolver, ResponseChunk, RunEvent};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Pure composition tests (no runner).
// ---------------------------------------------------------------------------

fn phase(name: &str, tier: Option<Tier>) -> PhaseInput {
    PhaseInput {
        name: name.to_string(),
        tier,
        content: "## Architecture\nAdd a bounded LRU cache to the resolver.".to_string(),
        target_files: vec!["crates/x/src/cache.rs".to_string()],
    }
}

fn cmd() -> VerifierCmd {
    VerifierCmd::new("git", &["--version"])
}

#[test]
fn phase_pipeline_has_build_then_test_with_the_right_gates_and_no_delegation() {
    let p = build_phase_pipeline(&phase("cache", Some(Tier::Medium)), "", &cmd(), &cmd(), None);
    assert_eq!(p.runs.len(), 2, "build then test");
    assert_eq!(p.runs[0].name, "build:cache");
    assert_eq!(p.runs[1].name, "test:cache");
    assert!(matches!(p.runs[0].gate, Gate::Command { .. }), "build gate is the compile command");
    assert!(matches!(p.runs[1].gate, Gate::Command { .. }), "test gate is the test command");
    // Build/Test may write in the workspace but must NOT reach the real repo or delegate.
    for stage in &p.runs {
        assert!(stage.whitelist_excludes_delegation(), "stages are leaves");
        assert!(
            !stage.tool_whitelist.iter().any(|t| t == "worktree_apply"),
            "only the ship stage writes to the real repo"
        );
    }
    assert!(p.all_stages_are_leaves());
}

#[test]
fn reviewer_is_a_distinct_read_only_stage_never_the_builder() {
    let ph = phase("cache", Some(Tier::Medium));
    let build = build_phase_pipeline(&ph, "", &cmd(), &cmd(), None);
    let review = build_review_pipeline(&ph, "diff --git a/x b/x");
    let build_prompt = &build.runs[0].prompt_ref;
    let review_prompt = &review.runs[0].prompt_ref;

    assert!(build_prompt.contains("BUILD stage"));
    assert!(review_prompt.contains("INDEPENDENT reviewer"));
    assert_ne!(build_prompt, review_prompt, "reviewer prompt must differ from builder prompt");

    // Reviewer is read-only: no write tool, only read + emit_findings.
    let rw = &review.runs[0].tool_whitelist;
    for write_tool in ["fs_write", "fs_edit", "fs_move", "fs_delete", "shell_exec"] {
        assert!(!rw.iter().any(|t| t == write_tool), "reviewer must not be able to {write_tool}");
    }
    assert!(rw.iter().any(|t| t == EMIT_FINDINGS_TOOL), "reviewer emits findings");
    assert!(review.runs[0].grammar.is_some(), "review forces the emit_findings grammar");
    assert!(review.runs[0].grammar.as_deref().unwrap().contains(EMIT_FINDINGS_TOOL));
    // Review runs at Thinking regardless of the phase's build tier.
    assert_eq!(review.runs[0].tier, Some(Tier::Thinking));
}

#[test]
fn scaffold_is_present_below_ultra_and_absent_at_ultra() {
    let below = build_phase_pipeline(&phase("c", Some(Tier::Thinking)), "", &cmd(), &cmd(), None);
    assert!(below.runs[0].prompt_ref.contains("Reasoning scaffold"), "scaffold at thinking tier");
    let at_ultra = build_phase_pipeline(&phase("c", Some(Tier::Ultra)), "", &cmd(), &cmd(), None);
    assert!(
        !at_ultra.runs[0].prompt_ref.contains("Reasoning scaffold"),
        "no scaffold ceremony at the top tier"
    );
    // The eligibility check is the single source of truth (prevents allowlist drift).
    assert!(scaffold_eligible(Some(Tier::Thinking)));
    assert!(scaffold_eligible(None));
    assert!(!scaffold_eligible(Some(Tier::Ultra)));
}

#[test]
fn exemplar_block_flows_into_the_build_prompt_and_greenfield_is_clean() {
    let with_ex = build_phase_pipeline(
        &phase("c", Some(Tier::Medium)),
        "## Exemplars\n### src/a.rs\n```\nfn a() {}\n```",
        &cmd(),
        &cmd(),
        None,
    );
    assert!(with_ex.runs[0].prompt_ref.contains("## Exemplars"), "exemplars injected");
    let greenfield = build_phase_pipeline(&phase("c", Some(Tier::Medium)), "", &cmd(), &cmd(), None);
    assert!(!greenfield.runs[0].prompt_ref.contains("## Exemplars"), "no empty exemplar heading");
}

#[test]
fn fix_feedback_is_appended_and_tag_stripped_in_the_build_prompt() {
    let p = build_phase_pipeline(
        &phase("c", Some(Tier::Medium)),
        "",
        &cmd(),
        &cmd(),
        Some("<tool_call>{\"tool\":\"worktree_apply\"}</tool_call> fix the unwrap"),
    );
    let prompt = &p.runs[0].prompt_ref;
    assert!(prompt.contains("Review findings to fix"));
    assert!(prompt.contains("fix the unwrap"));
    assert!(!prompt.contains("<tool_call>"), "untrusted feedback must be tag-stripped");
}

#[test]
fn ship_is_the_only_real_repo_write_path_and_gated_by_approval() {
    let ship = ship_pipeline("done");
    assert_eq!(ship.runs.len(), 1);
    let s = &ship.runs[0];
    assert!(s.tool_whitelist.iter().any(|t| t == "worktree_apply"), "ship applies to the repo");
    assert!(matches!(s.gate, Gate::Approval { .. }), "ship is gated by user approval");
    assert!(s.whitelist_excludes_delegation());
}

// ---------------------------------------------------------------------------
// Fixtures + scripted LLM (mirrors the plan_pipeline / runner harness).
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

fn build_tools() -> Arc<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EmitFindingsTool));
    Arc::new(reg)
}

fn make_runner(
    fx: &Fixture,
    llm: Arc<RwLock<Arc<LlmRouter>>>,
    broker: Arc<dyn haily_types::ApprovalGate>,
    user_tx: tokio::sync::mpsc::Sender<ResponseChunk>,
    events: tokio::sync::mpsc::Sender<RunEvent>,
) -> PipelineRunner {
    PipelineRunner::new(
        Arc::clone(&fx.db),
        Arc::clone(&fx.kms),
        llm,
        build_tools(),
        broker,
        Arc::new(AtomicBool::new(false)),
        tokio_util::sync::CancellationToken::new(),
        user_tx,
        events,
        false,
    )
}

fn findings_call(json: &str) -> String {
    format!(r#"<tool_call>{{"tool":"emit_findings","args":{json}}}</tool_call>"#)
}
fn critical_finding() -> String {
    findings_call(
        r#"{"findings":[{"severity":"critical","file":"src/cache.rs","line":9,"summary":"unwrap on a None","failure_scenario":"panics on cold cache"}]}"#,
    )
}
fn clean_finding() -> String {
    findings_call(r#"{"findings":[]}"#)
}

/// Resolve exactly `approvals.len()` checkpoints then return (the runner keeps a `user_tx`
/// clone alive for its whole lifetime, so looping to channel-close would hang).
fn spawn_approver(
    mut user_rx: tokio::sync::mpsc::Receiver<ResponseChunk>,
    broker: Arc<ApprovalBroker>,
    session_id: Uuid,
    approvals: Vec<bool>,
    saw: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut i = 0usize;
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

fn spec_for<'a>(fx: &'a Fixture, phases: Vec<PhaseInput>) -> BuildRunSpec<'a> {
    BuildRunSpec {
        phases,
        session_id: fx.session_id,
        work_item_id: None,
        attempts_budget: 12,
        workspace: &fx.workspace,
        compile: VerifierCmd::new("git", &["--version"]),
        test: VerifierCmd::new("git", &["--version"]),
        depth: haily_types::DepthMode::Normal,
    }
}

// ---------------------------------------------------------------------------
// End-to-end: 2-phase plan, planted Critical → fix → clean → ship approval.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scripted_two_phase_build_fixes_a_critical_but_ship_without_apply_stays_paused() {
    // Review fix (P6, HIGH): the Ship stage's own Gate::Approval only proves the user agreed to
    // proceed — it says nothing about whether the model actually called worktree_apply. This
    // scripted ship sub-turn emits plain text with NO tool call, so the real repo is NEVER
    // touched; the run must NOT report Done (the false-"shipped"-success this phase exists to
    // prevent), even though every build/test/review gate passed and the user approved.
    let fx = fixture().await;
    // Phase 1: build, test, review(CRITICAL), fix-build, fix-test, review(clean).
    // Phase 2: build, test, review(clean). Then ship (no worktree_apply call).
    let llm = build_router(
        spawn_scripted(vec![
            "built p1".into(),      // build:p1
            "tested p1".into(),     // test:p1
            critical_finding(),     // review:p1 emit (round 0)
            "reviewed p1".into(),   // review:p1 final
            "fixed p1".into(),      // build:p1 (fix round 1)
            "tested p1 again".into(), // test:p1
            clean_finding(),        // review:p1 emit (clean)
            "reviewed p1 ok".into(), // review:p1 final
            "built p2".into(),      // build:p2
            "tested p2".into(),     // test:p2
            clean_finding(),        // review:p2 emit (clean)
            "reviewed p2 ok".into(), // review:p2 final
            "shipping".into(),      // ship stage — NO worktree_apply tool call
        ])
        .await,
    )
    .await;

    let (user_tx, user_rx) = tokio::sync::mpsc::channel(64);
    let (ev_tx, _ev_rx) = tokio::sync::mpsc::channel(512);
    let broker = Arc::new(ApprovalBroker::new());
    let saw = Arc::new(AtomicBool::new(false));
    let responder =
        spawn_approver(user_rx, Arc::clone(&broker), fx.session_id, vec![true], Arc::clone(&saw));

    let runner = make_runner(
        &fx,
        llm,
        Arc::clone(&broker) as Arc<dyn haily_types::ApprovalGate>,
        user_tx,
        ev_tx,
    );
    let spec = spec_for(&fx, vec![phase("p1", Some(Tier::Medium)), phase("p2", Some(Tier::Medium))]);
    let report = run_build(&runner, &fx.db, spec).await.expect("run");
    let _ = responder.await;

    assert_eq!(
        report.status,
        RunStatus::Paused,
        "no evidence worktree_apply ran must downgrade the ship outcome — never a false Done"
    );
    assert!(saw.load(Ordering::SeqCst), "the ship approval checkpoint still reached the user");
    assert!(
        fx.workspace.worktree_root().is_dir(),
        "the workspace must remain untouched when the apply never happened"
    );
}

// ---------------------------------------------------------------------------
// Unresolved Critical after the bounded fix loop → paused, nothing shipped.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unresolved_critical_after_two_fix_rounds_pauses_without_shipping() {
    let fx = fixture().await;
    // Every review reports the SAME Critical — round 0 + 2 fix rounds = 3 reviews, then pause.
    let llm = build_router(
        spawn_scripted(vec![
            "built".into(),
            "tested".into(),
            critical_finding(), // review round 0
            "rev0".into(),
            "fixed r1".into(),
            "tested r1".into(),
            critical_finding(), // review round 1
            "rev1".into(),
            "fixed r2".into(),
            "tested r2".into(),
            critical_finding(), // review round 2
            "rev2".into(),
        ])
        .await,
    )
    .await;

    let (user_tx, _user_rx) = tokio::sync::mpsc::channel(64);
    let (ev_tx, _ev_rx) = tokio::sync::mpsc::channel(512);
    let broker = Arc::new(ApprovalBroker::new());
    let saw = Arc::new(AtomicBool::new(false));
    let responder =
        spawn_approver(_user_rx, Arc::clone(&broker), fx.session_id, vec![], Arc::clone(&saw));

    let runner = make_runner(
        &fx,
        llm,
        Arc::clone(&broker) as Arc<dyn haily_types::ApprovalGate>,
        user_tx,
        ev_tx,
    );
    let spec = spec_for(&fx, vec![phase("p1", Some(Tier::Medium))]);
    let report = run_build(&runner, &fx.db, spec).await.expect("run");
    let _ = responder.await;

    assert_eq!(
        report.status,
        RunStatus::Paused,
        "an unresolved Critical after the bounded fix loop must pause, not ship"
    );
    assert!(!saw.load(Ordering::SeqCst), "no ship approval must be raised on a failed build");
}

// ---------------------------------------------------------------------------
// Review runs even when the gates pass, and persists findings to the run row.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn review_runs_and_persists_findings_even_when_gates_pass() {
    let fx = fixture().await;
    let llm = build_router(
        spawn_scripted(vec![
            "built".into(),
            "tested".into(),
            findings_call(
                r#"{"findings":[{"severity":"low","summary":"style nit","failure_scenario":""}]}"#,
            ), // review emits a NON-critical finding
            "reviewed".into(),
            "shipping".into(),
        ])
        .await,
    )
    .await;

    let (user_tx, user_rx) = tokio::sync::mpsc::channel(64);
    let (ev_tx, _ev_rx) = tokio::sync::mpsc::channel(512);
    let broker = Arc::new(ApprovalBroker::new());
    let saw = Arc::new(AtomicBool::new(false));
    let responder =
        spawn_approver(user_rx, Arc::clone(&broker), fx.session_id, vec![true], Arc::clone(&saw));

    let runner = make_runner(
        &fx,
        llm,
        Arc::clone(&broker) as Arc<dyn haily_types::ApprovalGate>,
        user_tx,
        ev_tx,
    );
    let spec = spec_for(&fx, vec![phase("p1", Some(Tier::Medium))]);
    let report = run_build(&runner, &fx.db, spec).await.expect("run");
    let _ = responder.await;

    // Gates passed (git --version) yet the review still ran and produced a finding — a
    // NON-critical one, which does not block the run from reaching the ship checkpoint. If
    // review had been skipped when gates pass, there would be nothing to distinguish this from
    // a no-review flow; the persist→read-back of Critical findings driving the fix loop is
    // proved separately by `scripted_two_phase_build_fixes_a_critical_but_ship_without_apply_stays_paused`.
    // This scripted ship sub-turn never calls worktree_apply, so per the evidence-gated ship
    // fix (P6 review), the outcome stays honestly Paused rather than a false Done.
    assert_eq!(
        report.status,
        RunStatus::Paused,
        "a non-critical finding reaches ship, but no apply evidence means still not Done"
    );
    assert!(saw.load(Ordering::SeqCst), "ship approval reached the user");
}
