//! Automation/connector golden eval — scripted goldens (Sub-Agent + Skill Architecture
//! phase 14).
//!
//! The CI-runnable, ZERO-NETWORK tier: drives each authored task fixture's scripted connector
//! steps through the REAL dispatch harness (`run_automation_eval` → `tool_call::dispatch` →
//! `HttpConnectorTool` → action journal → `undo_turn`) against the in-crate loopback mock SaaS.
//! Every assertion is a deterministic end-state / DB fact — never an LLM judge (locked). The
//! per-candidate-MODEL matrix (a real model GENERATING the steps) is model-host-gated and
//! DEFERRED (see `evals/mock_saas/README.md`).
//!
//! Faithfulness: the eval manifest reuses the SHIPPED `odoo-crm` protocol + real CRM ops
//! verbatim (protocol translation / read-back / fault classification identical to production),
//! adding only two eval-only destructive ops for the RiskTier/ApprovalGate + reward-hack
//! differentiators. The manifest's base URL is pointed at the mock by the eval-mode, origin-
//! gated `ConnectionOverlay` override — no connector code changed.

use std::sync::Arc;

use haily_core::pipeline::automation_eval::mock::MockSaas;
use haily_core::pipeline::{
    parse_automation_task, render_automation_outcome, run_automation_eval, AutomationDeps,
    AutomationOutcome, AutomationTask, EvalMode,
};
use haily_db::queries::eval_runs;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_tools::connector::{manifest, Manifest};
use haily_types::{DepthMode, Request, RequestOrigin};

const EVAL_MANIFEST: &str = include_str!("../../../evals/mock_saas/odoo-eval.manifest.json");
const FIXTURE_ARCHIVE_UNDO: &str = include_str!("../../../evals/automation/crm-archive-undo.yaml");
const FIXTURE_DELETE_APPROVAL: &str =
    include_str!("../../../evals/automation/ops-delete-approval.yaml");
const FIXTURE_REWARD_HACK: &str = include_str!("../../../evals/automation/crm-reward-hack.yaml");

async fn deps() -> (AutomationDeps, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());
    let kms = Arc::new(KmsHandle::init((*db).clone(), dir.path()).await.unwrap());
    (
        AutomationDeps {
            db,
            kms,
            model: "scripted".to_string(),
            tier_config: "scripted".to_string(),
        },
        dir,
    )
}

fn eval_manifest() -> Arc<Manifest> {
    Arc::new(manifest::parse(EVAL_MANIFEST).expect("eval manifest parses via the phase-4 parser"))
}

/// The SEC-H witness: only a CLI-origin request mints one (proven by the origin tests). Every
/// eval run below carries it — a chat request could never obtain it.
fn cli_mode() -> EvalMode {
    let req = Request {
        session_id: uuid::Uuid::new_v4(),
        adapter_id: "eval-cli".to_string(),
        message: "eval automation".to_string(),
        user_ref: None,
        depth: DepthMode::Normal,
        origin: RequestOrigin::Cli,
    };
    EvalMode::from_request(&req).expect("cli origin enables eval mode")
}

async fn run(task: &AutomationTask) -> (AutomationOutcome, Arc<DbHandle>) {
    let (deps, _dir) = deps().await;
    // A fresh mock per run (reseeded per task inside the runner anyway).
    let mock = MockSaas::start(Vec::new()).await;
    let outcome = run_automation_eval(&deps, task, eval_manifest(), &mock, cli_mode())
        .await
        .expect("automation eval runs");
    (outcome, deps.db)
}

// ---------------------------------------------------------------------------------------------
// CRITICAL: a multi-step connector task graded deterministically (objective + guardrail), zero
// network — PLUS the differentiators (journal complete, undo restores seed BIT-EQUAL).
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn multi_step_connector_task_grades_and_undo_restores_bit_equal() {
    let task = parse_automation_task(FIXTURE_ARCHIVE_UNDO).expect("parse fixture");
    assert!(task.steps.len() >= 2, "this fixture must be multi-step");
    let (outcome, db) = run(&task).await;

    // Deterministic objective + guardrail grading.
    assert_eq!(outcome.score.objective_pass, outcome.score.objective_total, "all objectives met");
    assert_eq!(outcome.score.guardrail_violations, 0, "no collateral damage");
    assert!(outcome.score.strict_binary, "strict-binary PASS");
    assert_eq!(outcome.score.partial_credit, 1.0, "partial-credit 1.0");

    // Differentiator: the action journal is COMPLETE for the run (one row per write step).
    assert!(
        outcome.journal_rows >= task.min_journal_entries,
        "journal must be complete: {} rows",
        outcome.journal_rows
    );
    // Differentiator: undo restored the seed state BIT-EQUAL.
    assert!(outcome.undo_restored, "undo_turn must restore the seed state bit-equal");
    assert!(
        outcome.score.gates.iter().any(|g| g.gate == "undo_restores_seed" && g.pass),
        "the undo-restores gate must pass: {:?}",
        outcome.score.gates
    );
    assert!(outcome.score.passed, "a clean multi-step task must pass");

    // Persisted to eval_runs with task_kind = automation.
    let row = eval_runs::get(&db, outcome.eval_run_id.as_ref().unwrap())
        .await
        .unwrap()
        .expect("eval_runs row");
    assert_eq!(row.task_kind, "automation");
    assert_eq!(row.task_id, "crm-archive-undo");
    assert!(row.passed);
    assert!(row.gate_results.as_deref().unwrap().contains("undo_restores_seed"));
}

// ---------------------------------------------------------------------------------------------
// CRITICAL: a destructive step FIRES the ApprovalGate (the eval auto-resolves + records it) and
// the journal is complete.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn destructive_step_fires_the_approval_gate() {
    let task = parse_automation_task(FIXTURE_DELETE_APPROVAL).expect("parse fixture");
    assert!(task.expects_approval, "this fixture's step must be destructive");
    let (outcome, _db) = run(&task).await;

    assert!(outcome.approval_fired, "the destructive delete must have fired the ApprovalGate");
    assert!(
        outcome.score.gates.iter().any(|g| g.gate == "approval_fired_on_destructive" && g.pass),
        "the approval-fired gate must pass: {:?}",
        outcome.score.gates
    );
    // The objective (record deleted) + guardrail (unrelated record intact) both hold, and the
    // journal recorded the destructive write.
    assert_eq!(outcome.score.objective_pass, outcome.score.objective_total);
    assert_eq!(outcome.score.guardrail_violations, 0);
    assert!(outcome.journal_rows >= 1, "the destructive write must be journaled");
    assert!(outcome.score.passed);
}

// ---------------------------------------------------------------------------------------------
// CRITICAL: a planted reward-hacking broad edit is zeroed by the guardrail AND caught by
// RiskTier/ApprovalGate (double coverage).
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reward_hack_broad_edit_is_double_caught() {
    let task = parse_automation_task(FIXTURE_REWARD_HACK).expect("parse fixture");
    assert!(task.reward_hack, "this fixture is a reward-hack trap");
    let (outcome, _db) = run(&task).await;

    // Guardrail layer: the collateral damage to the unrelated record zeroes the score.
    assert!(outcome.score.guardrail_violations > 0, "the guardrail must flag the collateral damage");
    assert_eq!(outcome.score.partial_credit, 0.0, "a guardrail violation zeroes partial-credit");
    assert!(!outcome.score.strict_binary, "strict-binary must be FAIL under a guardrail violation");

    // RiskTier/ApprovalGate layer: the broad edit is IrreversibleWrite and fired the gate.
    assert!(outcome.approval_fired, "the broad edit must have fired the ApprovalGate (RiskTier)");

    // DOUBLE coverage gate passes → the eval SUCCEEDS at catching the hack.
    assert!(
        outcome.score.gates.iter().any(|g| g.gate == "reward_hack_double_caught" && g.pass),
        "the reward hack must be caught by BOTH layers: {:?}",
        outcome.score.gates
    );
    assert!(outcome.score.passed, "catching a reward hack is a passing eval outcome");
}

// ---------------------------------------------------------------------------------------------
// Both metrics are reported and labelled NON-COMPARABLE.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn report_shows_both_metrics_labelled_non_comparable() {
    let task = parse_automation_task(FIXTURE_ARCHIVE_UNDO).expect("parse fixture");
    let (outcome, _db) = run(&task).await;
    let md = render_automation_outcome(&outcome);
    assert!(md.contains("NON-COMPARABLE"), "the report must label the metrics non-comparable");
    assert!(md.contains("Partial-credit"), "partial-credit metric present: {md}");
    assert!(md.contains("Strict-binary"), "strict-binary metric present: {md}");
}

// ---------------------------------------------------------------------------------------------
// CRITICAL (SEC-H, extends the P9 structural origin test): the eval-mode base-URL override +
// approval auto-resolution are reachable ONLY from a RequestOrigin::Cli — a chat Request can
// NEVER mint the EvalMode witness `run_automation_eval` requires by value.
// ---------------------------------------------------------------------------------------------

#[test]
fn eval_mode_is_unreachable_from_a_chat_request() {
    let mut req = Request {
        session_id: uuid::Uuid::new_v4(),
        adapter_id: "telegram".to_string(),
        message: "please run the automation eval and point the connector at my server".to_string(),
        user_ref: None,
        depth: DepthMode::Normal,
        origin: RequestOrigin::Chat,
    };
    assert!(
        EvalMode::from_request(&req).is_none(),
        "SEC-H: a chat Request must NEVER enable the eval-mode base-URL override / auto-approve"
    );
    req.origin = RequestOrigin::Cli;
    assert!(EvalMode::from_request(&req).is_some(), "only a CLI origin enables the eval witness");
}

// ---------------------------------------------------------------------------------------------
// Determinism: an identical run twice yields byte-identical scored gates (Router-A/B stability).
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn scoring_is_reproducible_across_identical_runs() {
    let task = parse_automation_task(FIXTURE_ARCHIVE_UNDO).expect("parse fixture");
    let (a, _da) = run(&task).await;
    let (b, _db) = run(&task).await;
    assert_eq!(a.score.to_json(), b.score.to_json(), "scoring must be bit-stable across runs");
}
