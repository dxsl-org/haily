//! Automation/connector golden eval (Sub-Agent + Skill Architecture phase 14) — AutomationBench
//! METHODOLOGY ported onto Haily's OWN connector surface.
//!
//! ## Why NOT a raw "Haily AutomationBench score" (recorded so it is not re-litigated)
//! AutomationBench drives a RAW model endpoint through its OWN agent loop against ITS OWN
//! simulated tools; it has no concept of an approval gate or reversibility. Point it at Haily's
//! model backend and every Haily differentiator — the manifest connectors, RiskTier,
//! ApprovalGate, the action journal + undo — is bypassed, and a correctly-behaving Haily that
//! PAUSES for approval on a destructive step is scored as non-completion (a SAFE agent scores
//! WORSE). So a headline "Haily got X%" from that harness measures the LLM, not the assistant.
//! Its AA per-model score is instead folded into P3 as a model→tier signal; its METHODOLOGY
//! (deterministic objective + guardrail assertions, no LLM-judge, reward-hacking guardrails) is
//! what this eval ports — measuring Haily's value-add, which AutomationBench structurally cannot.
//!
//! ## What this runner does
//! Drives each task's scripted connector steps through the REAL harness dispatch
//! ([`crate::tool_call::dispatch`] — RiskTier gating + ApprovalGate + kill switch + the action
//! journal), executed by the generic `HttpConnectorTool` against a local [`mock::MockSaas`]
//! (zero-network), then scores the end-state deterministically ([`scoring::score`]) and asserts
//! the differentiators: the approval gate fired on a destructive step, the journal is complete,
//! `undo_turn` restores the seed state bit-equal, and a reward-hacking broad edit is caught by
//! BOTH the guardrail assertions AND RiskTier/ApprovalGate.
//!
//! ## Two-tier deliverable (mirrors P9's honesty split)
//! - The CI-runnable tier drives scripted connector steps against the mock — green in
//!   `cargo test --workspace`, zero network (`crates/haily-core/tests/automation_goldens.rs`).
//! - The per-candidate-MODEL matrix (a real model GENERATING the tool calls) is model-host-gated
//!   → DEFERRED; the runner + fixtures + scoring + persistence are BUILT here, the matrix run is
//!   a documented manual step (see `evals/mock_saas/README.md`).
//!
//! ## SEC-H — origin gate (reused from P9)
//! The eval-mode connector base-URL override + approval auto-resolution are privileged and
//! reachable ONLY with an [`EvalMode`] witness, constructible only from a
//! [`haily_types::RequestOrigin::Cli`] request — a chat `Request` can NEVER obtain one, proven
//! by the origin tests. Connector code is UNCHANGED beyond this eval-mode-only base-URL override
//! (applied through the existing M4 `ConnectionOverlay`).
mod scoring;
mod task;

pub mod mock;

pub use scoring::{score as score_automation, AutomationScore, ScoreInputs};
pub use task::{parse_automation_task, AutomationTask, SeedRecord, StateAssertion, Step};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use haily_db::queries::eval_runs::{self, NewEvalRun};
use haily_db::queries::journal;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_tools::connector::{
    ConnectionOverlay, ConnectorExecutor, HttpConnectorTool, HttpExecutor, HttpExecutorConfig,
    Manifest,
};
use haily_tools::journal_undo::{undo_turn, ConnectorResolver};
use haily_tools::{ToolContext, ToolRegistry};
use haily_types::{DepthMode, ResponseChunk};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::approval::ApprovalBroker;
use crate::pipeline::eval_runner::EvalMode;
use crate::tool_call;

/// Content hash the eval pins into every journal row + the undo resolver. The mock manifest is
/// built in-memory (not through `connector_manifests`), so a fixed literal — used identically on
/// both sides — exercises the SAME hash-pin code path production wiring uses (mirrors the Odoo
/// golden's `TEST_MANIFEST_HASH`).
const EVAL_MANIFEST_HASH: &str = "automation-eval-manifest-hash";

/// Connector-call timeout for the eval (the mock replies instantly; this only bounds a hang).
const EVAL_CONNECTOR_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared handles + labels the automation eval persists against.
pub struct AutomationDeps {
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    /// Recorded on the `eval_runs` row — a GGUF/cloud model id, or `scripted` for the CI tier.
    pub model: String,
    /// Tier configuration label (`local` / `cloud` / `scripted`).
    pub tier_config: String,
}

/// The scored outcome of one automation task run.
pub struct AutomationOutcome {
    pub task_id: String,
    pub domain: String,
    pub score: AutomationScore,
    pub journal_rows: usize,
    pub approval_fired: bool,
    pub undo_restored: bool,
    /// Set once persisted to `eval_runs`.
    pub eval_run_id: Option<String>,
}

/// Run one automation task end-to-end against `mock`: reseed → drive the scripted connector
/// steps through the real dispatch harness → deterministic score → persisted `eval_runs`
/// (task_kind = `automation`). `_mode` is the SEC-H witness proving a CLI origin; its presence
/// is the authorization for the eval-mode connector base-URL override (`mock.base_url`) + the
/// approval auto-resolution below.
///
/// # Errors
/// Returns an error only for a setup/persistence failure; a failing assertion or a fired gate
/// is a normal scored outcome, never an error.
pub async fn run_automation_eval(
    deps: &AutomationDeps,
    task: &AutomationTask,
    manifest: Arc<Manifest>,
    mock: &mock::MockSaas,
    _mode: EvalMode,
) -> Result<AutomationOutcome> {
    // Per-task deterministic seed reset, then the seed digest (the bit-equal undo baseline).
    mock.reset(seed_records(task));
    let seed_digest = mock.digest();

    let session_id = Uuid::new_v4();
    haily_db::queries::sessions::create_session(&deps.db, &session_id.to_string(), "eval", None)
        .await?;
    let turn_id = Uuid::new_v4();

    // Eval-mode base-URL override (origin-gated by `_mode`): the existing M4 overlay points the
    // manifest at the loopback mock, and the TEST-ONLY `allow_loopback` lets the SSRF guard
    // permit it. Production wiring never sets either — this path is unreachable without EvalMode.
    let kill = Arc::new(AtomicBool::new(false));
    // `AcceptEdits`: the eval must still see the IrreversibleWrite approval prompt FIRE (the
    // `approval_fired` differentiator assertion below) — `Auto` mode would skip it entirely.
    let approval_mode =
        crate::permission_mode::new_handle(crate::permission_mode::ApprovalMode::AcceptEdits);
    let overlay = ConnectionOverlay {
        base_url_override: Some(mock.base_url.clone()),
        db: Some("eval".to_string()),
        uid: Some(1),
        cred_ref_override: None,
    };
    let mut cfg = HttpExecutorConfig::production(
        Arc::clone(&manifest),
        Arc::clone(&kill),
        EVAL_CONNECTOR_TIMEOUT,
    )
    .with_connection_overlay(Some(overlay));
    cfg.allow_loopback = true; // eval-only; gated by EvalMode, never set in production.
    let executor: Arc<dyn ConnectorExecutor> = Arc::new(HttpExecutor::new(cfg));

    let registry = build_registry(&manifest, Arc::clone(&executor), Arc::clone(&kill));
    let resolver =
        ConnectorResolver::for_manifest(&manifest, Arc::clone(&executor), EVAL_MANIFEST_HASH);

    // Real broker + an eval auto-responder: APPROVES every connector approval request (so a
    // destructive step actually executes and its collateral damage is observable to the
    // guardrail) while recording that the gate FIRED — the origin-gated eval policy (LOCKED 3a).
    let broker = Arc::new(ApprovalBroker::new());
    let (approval_tx, approval_rx) = mpsc::channel::<ResponseChunk>(64);
    let approval_fired = Arc::new(AtomicBool::new(false));
    let responder = spawn_eval_auto_responder(
        approval_rx,
        Arc::clone(&broker),
        session_id,
        Arc::clone(&approval_fired),
    );

    let cancel = CancellationToken::new();
    let ctx = ToolContext {
        db: Arc::clone(&deps.db),
        kms: Arc::clone(&deps.kms),
        session_id,
        turn_id,
        depth: 0,
        domain: None,
        approval_gate: Arc::clone(&broker) as Arc<dyn haily_types::ApprovalGate>,
        approval_tx,
        cancel: cancel.clone(),
        turn_deletes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        last_journal_id: Arc::new(std::sync::Mutex::new(None)),
        run_id: None,
        depth_mode: DepthMode::Normal,
        // View Engine Phase A (phase 3): this eval harness is deliberately isolated from the
        // Orchestrator (its own throwaway `kill`/`broker` above, never the live ones) and its
        // scripted connector steps never include a view-producing tool — a fresh, call-scoped
        // store is therefore correct here, unlike `run_turn`/`run_sub_turn`, which now share
        // the Orchestrator's ONE `ViewStore` (see `Orchestrator::view_store`).
        view_sink: Arc::new(crate::view::ViewStore::new()),
    };

    // Drive the scripted connector steps through the REAL dispatch harness (a failing step is a
    // scored outcome, never an early return — the objective/guardrail assertions judge it).
    for step in &task.steps {
        let _ = tool_call::dispatch(
            &step.tool,
            step.params.clone(),
            &registry,
            &ctx,
            &kill,
            &approval_mode,
        )
        .await;
    }
    drop(ctx); // drops the only approval_tx → the responder's channel closes.
    let _ = responder.await;
    let approval_fired = approval_fired.load(Ordering::SeqCst);

    // Deterministic end-state scoring by DIRECT reads of the mock (the writes travelled the real
    // connector network path; only the scoring read is in-process).
    let (objective_pass, objective_total) = eval_assertions(mock, &task.objective_assertions);
    let (guardrail_pass, guardrail_total) = eval_assertions(mock, &task.guardrail_assertions);
    let guardrail_violations = guardrail_total - guardrail_pass;

    let journal_rows =
        journal::list_by_turn(&deps.db, &turn_id.to_string(), &session_id.to_string())
            .await
            .map(|r| r.len())
            .unwrap_or(0);

    // Undo the whole turn (connector rows journal by turn_id, NOT run_id — see the Deviation
    // Log), then assert the seed state is restored bit-equal.
    let _ = undo_turn(
        &deps.db,
        &deps.kms,
        &resolver,
        &turn_id.to_string(),
        &session_id.to_string(),
    )
    .await;
    let undo_restored = mock.digest() == seed_digest;

    let score = scoring::score(&ScoreInputs {
        objective_pass,
        objective_total,
        guardrail_violations,
        approval_fired,
        expects_approval: task.expects_approval,
        journal_rows,
        min_journal_entries: task.min_journal_entries,
        undo_restored,
        expects_undo_restores: task.expects_undo_restores,
        reward_hack: task.reward_hack,
        // For a reward-hack task the only gated op IS the broad edit, so a fired gate is proof
        // the hack was RiskTier-gated (the runtime half of the double coverage).
        reward_hack_risk_gated: approval_fired,
    });

    let mut outcome = AutomationOutcome {
        task_id: task.id.clone(),
        domain: task.domain.clone(),
        score,
        journal_rows,
        approval_fired,
        undo_restored,
        eval_run_id: None,
    };
    outcome.eval_run_id = Some(persist(deps, &outcome).await?);
    Ok(outcome)
}

/// Build a registry with one `HttpConnectorTool` per manifest op — the exact per-op wiring
/// `register_connectors` builds in production (minus the credential getter; the mock needs none).
fn build_registry(
    manifest: &Arc<Manifest>,
    executor: Arc<dyn ConnectorExecutor>,
    kill: Arc<AtomicBool>,
) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    for op in &manifest.ops {
        reg.register(Arc::new(HttpConnectorTool {
            manifest: Arc::clone(manifest),
            op: Arc::new(op.clone()),
            executor: Arc::clone(&executor),
            kill: Arc::clone(&kill),
            cred_ref: format!("connector.{}.api_key", manifest.connector_name),
            manifest_hash: EVAL_MANIFEST_HASH.to_string(),
        }));
    }
    reg
}

/// The eval auto-responder: approves EVERY connector approval request (letting a destructive
/// step run so the guardrail can observe its effect) and records that the gate fired. Exits when
/// the runner drops its `approval_tx`.
fn spawn_eval_auto_responder(
    mut rx: mpsc::Receiver<ResponseChunk>,
    broker: Arc<ApprovalBroker>,
    session_id: Uuid,
    fired: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    use haily_types::ApprovalResolver;
    tokio::spawn(async move {
        while let Some(chunk) = rx.recv().await {
            if let ResponseChunk::ToolApprovalRequest { approval_id, .. } = chunk {
                fired.store(true, Ordering::SeqCst);
                broker.resolve(approval_id, session_id, true);
            }
        }
    })
}

/// Map a task's declared seed records to the mock's seed shape.
fn seed_records(task: &AutomationTask) -> Vec<mock::SeedRecord> {
    task.seed_state
        .iter()
        .map(|r| mock::SeedRecord {
            model: r.model.clone(),
            reference: r.reference.clone(),
            fields: r.fields.clone(),
        })
        .collect()
}

/// Evaluate a set of assertions against the mock's current state, returning (passed, total).
fn eval_assertions(mock: &mock::MockSaas, assertions: &[StateAssertion]) -> (usize, usize) {
    let st = mock.state.lock().unwrap_or_else(|e| e.into_inner());
    let mut pass = 0;
    for a in assertions {
        let matches = st.find(&a.model, &a.match_field, &a.match_value);
        if a.holds(&matches) {
            pass += 1;
        }
    }
    (pass, assertions.len())
}

/// Persist one scored automation run to `eval_runs` (task_kind = `automation`), returning its id.
async fn persist(deps: &AutomationDeps, outcome: &AutomationOutcome) -> Result<String> {
    let gate_results_json = Some(outcome.score.to_json());
    let row = eval_runs::insert(
        &deps.db,
        NewEvalRun {
            task_id: &outcome.task_id,
            task_kind: "automation",
            model: &deps.model,
            tier_config: &deps.tier_config,
            depth: "normal",
            per_stage_tokens: None,
            escalation_count: 0,
            egress: None,
            wall_clock_ms: 0,
            passed: outcome.score.passed,
            gate_results: gate_results_json.as_deref(),
        },
    )
    .await?;
    Ok(row.id)
}

/// Render an automation outcome as a markdown report section — the two NON-COMPARABLE headline
/// metrics (labelled) plus the differentiator gate table.
pub fn render_automation_outcome(outcome: &AutomationOutcome) -> String {
    let s = &outcome.score;
    let mut out = String::new();
    out.push_str(&format!(
        "## Automation eval: {} ({})\n\n",
        outcome.task_id, outcome.domain
    ));
    out.push_str(
        "> Metrics are NON-COMPARABLE (AutomationBench's two orgs, two lenses, one task set):\n\n",
    );
    out.push_str(&format!(
        "- Partial-credit (Artificial-Analysis lens): {:.2} ({}/{} objectives, {} guardrail violation(s))\n",
        s.partial_credit, s.objective_pass, s.objective_total, s.guardrail_violations
    ));
    out.push_str(&format!(
        "- Strict-binary (Zapier lens): {}\n",
        if s.strict_binary { "PASS" } else { "FAIL" }
    ));
    out.push_str(&format!(
        "- **Verdict: {}**\n\n",
        if s.passed { "PASS" } else { "FAIL" }
    ));
    out.push_str("| Differentiator gate | Result | Detail |\n|---|---|---|\n");
    for g in &s.gates {
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            g.gate,
            if g.pass { "PASS" } else { "FAIL" },
            g.detail
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_types::{Request, RequestOrigin};

    // SEC-H (LOCKED 1): the automation eval — like the coding eval — is reachable ONLY with an
    // EvalMode witness, which a chat-origin Request can NEVER mint. `run_automation_eval` takes
    // `EvalMode` by value, so this is a compile-time guarantee reinforced at runtime here.
    #[test]
    fn a_chat_origin_request_can_never_obtain_the_eval_witness() {
        let chat = Request {
            session_id: Uuid::new_v4(),
            adapter_id: "telegram".to_string(),
            message: "run the automation eval".to_string(),
            user_ref: None,
            depth: DepthMode::Normal,
            origin: RequestOrigin::Chat,
            forced_skill: None,
        };
        assert!(
            EvalMode::from_request(&chat).is_none(),
            "SEC-H: a chat Request must never enable the eval-mode base-URL override / auto-approve"
        );
        let mut cli = chat;
        cli.origin = RequestOrigin::Cli;
        assert!(
            EvalMode::from_request(&cli).is_some(),
            "only a CLI origin enables it"
        );
    }
}
