//! Golden Coding Eval — scripted-LLM goldens (Sub-Agent + Skill Architecture phase 9).
//!
//! The CI-runnable, ZERO-NETWORK tier of P9's two-tier deliverable: drives the REAL coding eval
//! runner (`run_coding_eval` → full plan→build→verify pipeline) against a scripted OpenAI-
//! compatible responder bound to `127.0.0.1:0` (the only "network", exactly like
//! `golden_tasks.rs`). Every assertion is a structural/DB fact — never an LLM judge (locked).
//!
//! These prove the eval-mode invariants end-to-end:
//! - eval mode NEVER writes outside the throwaway workspace nor applies to a real repo (ship is
//!   hard-blocked — `worktree_apply` is absent from the eval registry).
//! - scoring is reproducible/bit-stable for the scripted suite.
//! - egress + escalation-count + token telemetry populate every `eval_runs` row.
//! - an impossible planted gate reports Failure, not a hang.
//! - depth variants (Normal + Deep) both run to a scored, persisted outcome.
//!
//! The per-attempt retry / escalation / pause / undo_run decision paths are table- and
//! scripted-tested in `pipeline::runner::tests`, `pipeline::build_pipeline::tests`, and
//! `pipeline::plan_pipeline::tests` (the runner is the sole orchestrator; the eval reuses it
//! unchanged), so this suite focuses on the eval-mode surface those do not cover.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use haily_core::pipeline::{run_coding_eval, EvalDeps, EvalMode, TaskManifest};
use haily_db::queries::eval_runs;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::{LlmConfig, LlmRouter};
use haily_types::{DepthMode, Request, RequestOrigin};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A scripted OpenAI-compatible responder that returns "done" for EVERY completion — enough to
/// drive the pipeline offline. Stages needing a specific tool call (emit_plan_draft, emit_findings)
/// simply fail their artifact/findings gate and pause; the eval scores the FIXTURE gate + the
/// structural invariants independently of the pipeline's terminal status, so a paused pipeline is
/// a valid scored run. Deterministic (no wall-clock in the payload).
async fn spawn_scripted() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { break };
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let _ = stream.read(&mut buf).await;
                counter.fetch_add(1, Ordering::SeqCst);
                let payload =
                    serde_json::json!({ "choices": [{ "message": { "content": "done" } }] })
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

async fn deps(escalation: bool) -> (EvalDeps, Vec<tempfile::TempDir>) {
    let dbdir = tempfile::tempdir().unwrap();
    let db = Arc::new(DbHandle::init(&dbdir.path().join("t.db")).await.unwrap());
    let kms = Arc::new(KmsHandle::init((*db).clone(), dbdir.path()).await.unwrap());
    let llm = build_router(spawn_scripted().await).await;
    let deps = EvalDeps {
        db,
        kms,
        llm,
        model: "scripted-test-model".to_string(),
        tier_config: if escalation { "local+escalate" } else { "local" }.to_string(),
        escalation_enabled: escalation,
    };
    (deps, vec![dbdir])
}

/// Write a throwaway fixture dir with a `task.yaml` (given gate) + one source file.
async fn write_fixture(id: &str, gate: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let manifest = format!(
        "id: {id}\nlanguage: rust\nkind: fix-compile-error\ndescription: \"do the thing\"\ngate: {gate}\nmax_tool_calls: 8\nmax_escalations: 0\ntimeout_seconds: 60\n"
    );
    tokio::fs::write(dir.path().join("task.yaml"), manifest).await.unwrap();
    tokio::fs::write(dir.path().join("main.txt"), "fixture content\n").await.unwrap();
    dir
}

fn manifest(id: &str, gate: &str) -> TaskManifest {
    haily_core::pipeline::parse_task_yaml(&format!(
        "id: {id}\nlanguage: rust\nkind: fix-compile-error\ndescription: \"do the thing\"\ngate: {gate}\nmax_tool_calls: 8\nmax_escalations: 0\ntimeout_seconds: 60\n"
    ))
    .unwrap()
}

fn cli_mode() -> EvalMode {
    let req = Request {
        session_id: uuid::Uuid::new_v4(),
        adapter_id: "eval-cli".to_string(),
        message: "eval".to_string(),
        user_ref: None,
        depth: DepthMode::Normal,
        origin: RequestOrigin::Cli,
        forced_skill: None,
    };
    EvalMode::from_request(&req).expect("cli origin enables eval mode")
}

// ---------------------------------------------------------------------------
// A passing gate → PASS, ship never applied, fixture original untouched, telemetry populated.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn eval_passes_scores_cleanly_without_touching_the_real_repo() {
    let (deps, _dirs) = deps(false).await;
    // `git --version` always exits 0 — a deterministic passing gate independent of any edit.
    let fixture = write_fixture("golden-pass", "git --version").await;
    let m = manifest("golden-pass", "git --version");

    let before = fixture_hash(fixture.path()).await;
    let outcome = run_coding_eval(&deps, &m, fixture.path(), DepthMode::Normal, cli_mode())
        .await
        .expect("eval runs");

    assert!(outcome.score.passed, "a passing gate + intact workspace must score PASS: {:?}", outcome.score.gates);
    // CRITICAL: the fixture ORIGINAL is byte-unchanged — eval wrote nothing outside its throwaway.
    assert_eq!(before, fixture_hash(fixture.path()).await, "the committed fixture must never be mutated");
    // CRITICAL: ship never applied to a real repo.
    assert!(
        outcome.score.gates.iter().any(|g| g.gate == "ship_not_applied" && g.pass),
        "eval must never apply to a real repo"
    );
    // Telemetry populated (FMA-M2 egress + escalation + per-stage tokens).
    assert!(!outcome.egress.is_empty(), "per-attempt egress must be recorded");
    assert_eq!(outcome.escalation_count, 0, "no escalation with escalation disabled");
    assert!(!outcome.per_stage_tokens.is_empty(), "per-stage token records must be present");

    // The eval_runs row persisted with the telemetry.
    let row = eval_runs::get(&deps.db, outcome.eval_run_id.as_ref().unwrap())
        .await
        .unwrap()
        .expect("eval_runs row");
    assert_eq!(row.task_id, "golden-pass");
    assert_eq!(row.task_kind, "coding");
    assert!(row.passed);
    assert!(row.egress.as_deref().unwrap().contains("egress"));
    assert!(row.gate_results.as_deref().unwrap().contains("ship_not_applied"));
}

// ---------------------------------------------------------------------------
// Reproducible scoring: the same scripted run twice → byte-identical gate_results.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scripted_scoring_is_reproducible() {
    let (deps, _dirs) = deps(false).await;
    let fixture = write_fixture("golden-repro", "git --version").await;
    let m = manifest("golden-repro", "git --version");

    let a = run_coding_eval(&deps, &m, fixture.path(), DepthMode::Normal, cli_mode()).await.unwrap();
    let b = run_coding_eval(&deps, &m, fixture.path(), DepthMode::Normal, cli_mode()).await.unwrap();

    let ja = serde_json::to_string(&a.score.gates).unwrap();
    let jb = serde_json::to_string(&b.score.gates).unwrap();
    assert_eq!(ja, jb, "scoring must be bit-stable across identical scripted runs");
    assert_eq!(a.score.passed, b.score.passed);
}

// ---------------------------------------------------------------------------
// An impossible planted gate → Failure, not a hang.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn impossible_gate_reports_failure_not_a_hang() {
    let (deps, _dirs) = deps(false).await;
    // `git rev-parse --verify <bad-ref>` exits nonzero fast (git is present; the ref is not) —
    // an always-failing gate that the model can never satisfy.
    let gate = "git rev-parse --verify haily-eval-nonexistent-ref-xyz";
    let fixture = write_fixture("golden-impossible", gate).await;
    let m = manifest("golden-impossible", gate);

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        run_coding_eval(&deps, &m, fixture.path(), DepthMode::Normal, cli_mode()),
    )
    .await
    .expect("an impossible gate must not hang")
    .expect("eval runs");

    assert!(!outcome.score.passed, "an unsatisfiable gate must score FAIL");
    assert!(
        outcome.score.gates.iter().any(|g| g.gate == "builds_and_tests_pass" && !g.pass),
        "the builds/tests gate must be the failing one"
    );
    // Even a failed run persists a row (the measurement schema Router A/B needs).
    let row = eval_runs::get(&deps.db, outcome.eval_run_id.as_ref().unwrap()).await.unwrap();
    assert!(row.is_some(), "a failed eval still persists an eval_runs row");
}

// ---------------------------------------------------------------------------
// Depth variants both run to a scored, persisted outcome (Normal + Deep).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn depth_variants_both_produce_scored_runs() {
    let (deps, _dirs) = deps(false).await;
    let fixture = write_fixture("golden-depth", "git --version").await;
    let m = manifest("golden-depth", "git --version");

    for depth in [DepthMode::Normal, DepthMode::Deep] {
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(180),
            run_coding_eval(&deps, &m, fixture.path(), depth, cli_mode()),
        )
        .await
        .unwrap_or_else(|_| panic!("depth {depth:?} must not hang"))
        .expect("eval runs");
        assert_eq!(outcome.depth, depth.as_label());
        assert!(outcome.eval_run_id.is_some(), "depth {depth:?} run must persist a row");
    }
    // Both a Normal and a Deep row are on record for this task_kind.
    let rows = eval_runs::list_by_kind(&deps.db, "coding").await.unwrap();
    assert!(rows.len() >= 2, "both depth runs persisted");
    assert!(rows.iter().any(|r| r.depth == "normal"));
    assert!(rows.iter().any(|r| r.depth == "deep"));
}

async fn fixture_hash(dir: &std::path::Path) -> String {
    // Cheap content hash of the fixture's files (task.yaml + sources) for the unchanged check.
    use std::hash::{Hash, Hasher};
    let mut names: Vec<String> = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await.unwrap();
    while let Some(e) = rd.next_entry().await.unwrap() {
        let bytes = tokio::fs::read(e.path()).await.unwrap_or_default();
        names.push(format!("{}:{}", e.file_name().to_string_lossy(), String::from_utf8_lossy(&bytes)));
    }
    names.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    names.hash(&mut h);
    format!("{:016x}", h.finish())
}
