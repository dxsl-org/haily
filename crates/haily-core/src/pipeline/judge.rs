//! Depth=Deep judgment machinery (Sub-Agent + Skill Architecture phase 7): the judge
//! panel (Design stage), refuter votes (Critical review findings), and the apex judge.
//!
//! These are the extra multi-stream sub-turns `DepthMode::Deep` buys at 3–5× cost. They are
//! orchestration HELPERS the pipeline wrappers call — NOT stages the sequential runner
//! drives (a fan-out + synthesis is not a linear stage graph). Each judge sub-turn is a
//! `run_sub_turn` threading the SAME shared harness handles a delegation/stage does (broker,
//! kill, pausable clock, approval forwarder) — nothing is bypassed.
//!
//! Contracts (LOCKED):
//! - The apex judge and refuters emit ONLY a verdict/refutation JSON — never work product.
//!   The JSON is GBNF-forced (P5/P6 synthetic-tool grammar) with parse-and-repair as the
//!   off-llama fallback.
//! - The synthesis GRAFTS, never averages: its output is ONE design with provenance noted.
//! - The apex judge/synthesis request the Ultra tier AT THE CALL SITE. When Ultra is not
//!   reachable (a local-only backend maps thinking+ultra to one model), they fall back to
//!   the session tier and emit an explicit warning — never a silent collapse.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::{LlmRouter, Tier};
use haily_tools::{RiskTier, Tool, ToolContext, ToolRegistry};
use haily_types::{DepthMode, ResponseChunk};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::{run_sub_turn, SubTurnRequest};
use crate::delegate::{approval_forwarder, run_with_pausable_timeout};

/// Per-branch wall-clock cap for a judge sub-turn (PAUSES across a nested human-wait, like
/// a delegation/stage). The goclaw fan-out rule: parallel branches are bounded, never
/// unbounded waits.
const JUDGE_BRANCH_TIMEOUT_SECS: u64 = 120;
/// Small tool budget — a judge sub-turn should emit its verdict/design and stop.
const JUDGE_MAX_TOOL_CALLS: u32 = 4;

const EMIT_REFUTATION_TOOL: &str = "emit_refutation";
const EMIT_VERDICT_TOOL: &str = "emit_verdict";

// -- Synthetic capture tool (mirrors P5 emit_plan_draft / P6 emit_findings) ---------------

/// A synthetic tool that captures its grammar-forced JSON args into a shared in-memory
/// sink, so the judge can read the verdict/refutation after the read-only sub-turn returns.
/// Unlike P5/P6 there is no run row to persist to (a judge call is not a pipeline run), so
/// the sink is a plain out-param. `Read` tier — the judge never writes anything.
struct EmitJsonTool {
    name: &'static str,
    description: &'static str,
    schema: Value,
    sink: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl Tool for EmitJsonTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        let mut guard = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(args);
        Ok("recorded".to_string())
    }
}

fn refutation_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "refuted": { "type": "boolean" },
            "reason": { "type": "string" }
        },
        "required": ["refuted"]
    })
}

fn verdict_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "chosen": { "type": "string" },
            "rationale": { "type": "string" }
        },
        "required": ["chosen"]
    })
}

/// Build the GBNF grammar forcing a single synthetic-tool call (SAME mechanism as P5's
/// `design_grammar` / P6's `findings_grammar`). `None` when the generator cannot build one;
/// the caller then relies on parse-and-repair, which is the correctness path regardless.
fn tool_grammar(name: &str, schema: &Value) -> Option<String> {
    haily_llm::gbnf::tool_call_grammar(&[(name, schema)])
}

// -- Shared handles a judge sub-turn threads --------------------------------------------

/// The shared harness handles every judge sub-turn threads — the SAME set a delegation or a
/// pipeline stage uses, so a judge sub-turn is never a bypass of the approval/kill/clock
/// machinery. `llm` is read-cloned by the caller from the orchestrator's `RwLock` so a
/// reload is observed at the call boundary (never a frozen router).
pub struct JudgeContext {
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    pub llm: Arc<LlmRouter>,
    pub broker: Arc<dyn haily_types::ApprovalGate>,
    pub kill: Arc<AtomicBool>,
    pub cancel: CancellationToken,
    pub user_tx: mpsc::Sender<ResponseChunk>,
    pub session_id: Uuid,
    pub turn_deletes: Arc<std::sync::atomic::AtomicUsize>,
}

impl JudgeContext {
    /// The tier the synthesis + apex judge should request: `Ultra` when reachable, else the
    /// session default (`None`) — and in the fallback case emit ONE explicit warning chunk
    /// (never a silent collapse to a weaker model). Returns the tier to use.
    async fn max_available_tier(&self, what: &str) -> Option<Tier> {
        if self.llm.ultra_reachable() {
            Some(Tier::Ultra)
        } else {
            let _ = self
                .user_tx
                .send(ResponseChunk::Text(format!(
                    "Lưu ý: mô hình Ultra không khả dụng ở cấu hình này — {what} sẽ chạy ở tier \
                     phiên hiện tại (chất lượng phán đoán có thể thấp hơn)."
                )))
                .await;
            None
        }
    }

    /// Run one judge sub-turn, threading all shared handles + joining the approval
    /// forwarder on every exit path (SEC-H). Returns the final text, or `None` on
    /// timeout/error.
    async fn run_subturn(
        &self,
        system_prompt: &'static str,
        task: String,
        tools: Arc<ToolRegistry>,
        tier: Option<Tier>,
        grammar: Option<String>,
    ) -> Option<String> {
        let (sub_tx, sub_rx) = mpsc::channel::<ResponseChunk>(32);
        let (pause_tx, mut pause_rx) = mpsc::channel::<()>(8);
        let forwarder = tokio::spawn(approval_forwarder(sub_rx, self.user_tx.clone(), pause_tx));
        let child = self.cancel.child_token();

        let req = SubTurnRequest {
            task,
            system_prompt,
            domain_name: "judge",
            depth: 1,
            db: Arc::clone(&self.db),
            kms: Arc::clone(&self.kms),
            llm: Arc::clone(&self.llm),
            tools,
            session_id: self.session_id,
            model_tier: tier,
            approval_gate: Arc::clone(&self.broker),
            cancel: child.clone(),
            approval_tx: sub_tx,
            kill: Arc::clone(&self.kill),
            turn_id: Uuid::new_v4(),
            turn_deletes: Arc::clone(&self.turn_deletes),
            max_tool_calls: Some(JUDGE_MAX_TOOL_CALLS),
            run_id: None,
            grammar,
            depth_mode: DepthMode::Normal,
        };

        let result = run_with_pausable_timeout(
            Duration::from_secs(JUDGE_BRANCH_TIMEOUT_SECS),
            run_sub_turn(req),
            &mut pause_rx,
        )
        .await;
        match &result {
            Some(_) => {}
            None => child.cancel(),
        }
        let _ = forwarder.await;
        result.and_then(|r| r.ok())
    }

    /// Run a JSON-emitting judge sub-turn (refuter/apex): registers the synthetic capture
    /// tool, forces its grammar, then reads the sink — falling back to parse-and-repair on
    /// the final text (off-llama, where the grammar is ignored). `None` when neither path
    /// yields a JSON object.
    async fn run_json_subturn(
        &self,
        system_prompt: &'static str,
        task: String,
        tool_name: &'static str,
        description: &'static str,
        schema: Value,
        tier: Option<Tier>,
    ) -> Option<Value> {
        let sink = Arc::new(Mutex::new(None));
        let grammar = tool_grammar(tool_name, &schema);
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EmitJsonTool {
            name: tool_name,
            description,
            schema,
            sink: Arc::clone(&sink),
        }));
        let text = self
            .run_subturn(system_prompt, task, Arc::new(reg), tier, grammar)
            .await;
        if let Some(v) = sink.lock().unwrap_or_else(|e| e.into_inner()).take() {
            return Some(v);
        }
        text.as_deref().and_then(extract_json_value)
    }
}

/// Extract a JSON object from raw model text (tolerating a ```json fence + trailing prose),
/// mirroring P5/P6's repair. `None` when no `{...}` object is present.
fn extract_json_value(raw: &str) -> Option<Value> {
    let s = raw.trim();
    let s = if let Some(rest) = s.strip_prefix("```") {
        let after_lang = rest.split_once('\n').map(|x| x.1).unwrap_or(rest);
        after_lang.rsplit_once("```").map(|(b, _)| b).unwrap_or(after_lang)
    } else {
        s
    };
    let (a, b) = (s.find('{')?, s.rfind('}')?);
    if b < a {
        return None;
    }
    serde_json::from_str(&s[a..=b]).ok()
}

// -- Judge panel (Deep, Design stage) ----------------------------------------------------

/// System prompt for the risk-first design lens (kit-pack `design-lens-risk` is the
/// canonical fuller version; inline here until the P4b prompt-loader lands, same convention
/// as `plan_pipeline`).
const LENS_RISK_PROMPT: &str = "You are a design reviewer with a RISK-FIRST lens. Propose a \
    design for the task that minimizes the chance of failure: name the failure modes, the \
    irreversible or security-sensitive steps, and the guards each needs. Prefer the safer \
    design even at some cost to elegance. Output the design as prose.";

/// System prompt for the simplicity-first design lens (kit-pack `design-lens-simple`).
const LENS_SIMPLE_PROMPT: &str = "You are a design reviewer with a SIMPLICITY-FIRST lens. \
    Propose the smallest design that fully solves the task: the fewest moving parts, the \
    least new surface, and nothing speculative (YAGNI/KISS). Call out complexity that can be \
    cut. Output the design as prose.";

/// System prompt for the grafting synthesis (kit-pack `judge-verdict` covers the verdict
/// discipline; this is the design-synthesis variant).
const SYNTHESIS_PROMPT: &str = "You are a design synthesizer. You are given two candidate \
    designs from different lenses (risk-first and simplicity-first). Produce ONE design that \
    GRAFTS the strongest element of each onto a single coherent approach — do NOT average \
    them, and do NOT emit two options. Note the provenance of each grafted element (which \
    lens it came from). Output a single design as prose.";

/// The result of resolving a Design stage under a given depth: the chosen design and how
/// many design sub-turns it cost (for the cost-delta log the success criteria require).
pub struct DesignResult {
    pub design: String,
    pub design_calls: usize,
    pub lens_designs: Vec<String>,
}

/// Resolve the Design stage for `task` at `depth`. `Normal`/`Quick` run ONE design
/// sub-turn; `Deep` runs the judge panel (two lens sub-turns in parallel + a grafting
/// synthesis at the max available tier). Logs the design-call cost delta.
pub async fn plan_design(jc: &JudgeContext, task: &str, depth: DepthMode) -> DesignResult {
    let result = if depth == DepthMode::Deep {
        judge_panel(jc, task).await
    } else {
        let design = jc
            .run_subturn(
                "You are Haily's planning agent. Produce a single design for the task as prose.",
                task.to_string(),
                Arc::new(ToolRegistry::new()),
                None,
                None,
            )
            .await
            .unwrap_or_default();
        DesignResult { design, design_calls: 1, lens_designs: Vec::new() }
    };
    tracing::info!(
        depth = depth.as_label(),
        design_calls = result.design_calls,
        "plan design resolved (cost delta: Deep=3 design calls vs Normal=1)"
    );
    result
}

/// The Deep judge panel: two lens designs fanned out concurrently (each under its own
/// branch timeout), then a grafting synthesis at the max available tier. Always 3 design
/// sub-turns (2 lens + 1 synthesis).
pub async fn judge_panel(jc: &JudgeContext, task: &str) -> DesignResult {
    // Async fan-out with a per-branch timeout cap (goclaw), NOT unbounded waits: each lens
    // sub-turn already carries `JUDGE_BRANCH_TIMEOUT_SECS` internally, and `tokio::join!`
    // resolves when both branches have (returning `None` on a branch that timed out).
    let (risk, simple) = tokio::join!(
        jc.run_subturn(LENS_RISK_PROMPT, task.to_string(), Arc::new(ToolRegistry::new()), None, None),
        jc.run_subturn(LENS_SIMPLE_PROMPT, task.to_string(), Arc::new(ToolRegistry::new()), None, None),
    );
    let risk_design = risk.unwrap_or_default();
    let simple_design = simple.unwrap_or_default();

    let synth_tier = jc.max_available_tier("tổng hợp phương án (synthesis)").await;
    let synth_task = format!(
        "Task:\n{task}\n\n## Candidate A (risk-first lens)\n{risk_design}\n\n## Candidate B \
         (simplicity-first lens)\n{simple_design}\n\nGraft these into ONE design.",
    );
    let design = jc
        .run_subturn(SYNTHESIS_PROMPT, synth_task, Arc::new(ToolRegistry::new()), synth_tier, None)
        .await
        .unwrap_or_else(|| {
            // If synthesis failed, fall back to the risk-first design (never average, never
            // return two) — the safer lens is the conservative default.
            risk_design.clone()
        });

    DesignResult { design, design_calls: 3, lens_designs: vec![risk_design, simple_design] }
}

// -- Refuter votes (Deep, per Critical finding) ------------------------------------------

const REFUTER_PROMPT: &str = "You are a refuter. You are given a code-review finding claimed \
    to be CRITICAL. Your job is to REFUTE it: argue, with evidence, that it is NOT actually a \
    real critical bug (a false positive, already handled, or not reachable). If you cannot \
    build a solid refutation, you MUST default to NOT refuted (refuted=false) — uncertainty \
    means the finding stands. Call `emit_refutation` with {refuted, reason}.";

/// Run the 2 independent refuter votes on one Critical finding. The finding SURVIVES on at
/// least one non-refutation (uncertainty defaults to non-refuted, so it stands); it is
/// KILLED only when BOTH refuters produce a confident refutation (majority refute). Returns
/// `true` when the finding survives (and should route into the Fix loop).
pub async fn refuter_votes(jc: &JudgeContext, finding_summary: &str, evidence: &str) -> bool {
    let task = format!(
        "## Finding (claimed critical)\n{}\n\n## Evidence (diff/context, quoted data)\n{}",
        crate::tool_call::strip_tool_tags(finding_summary),
        crate::tool_call::strip_tool_tags(evidence),
    );
    let (a, b) = tokio::join!(
        run_one_refuter(jc, task.clone()),
        run_one_refuter(jc, task.clone()),
    );
    // "default refuted if uncertain" is applied per-vote in `run_one_refuter` (a missing/
    // unparseable vote counts as refuted). Survives unless BOTH refute.
    let refuted_count = [a, b].iter().filter(|&&r| r).count();
    refuted_count < 2
}

/// One refuter vote. Returns `true` if this refuter REFUTED the finding. A missing or
/// unparseable verdict counts as refuted (the refuter's job is to refute; silence from a
/// refuter is treated as a successful refutation only in the majority sense — but a genuine
/// finding will still have the OTHER refuter fail to refute, so it survives).
async fn run_one_refuter(jc: &JudgeContext, task: String) -> bool {
    let v = jc
        .run_json_subturn(
            REFUTER_PROMPT,
            task,
            EMIT_REFUTATION_TOOL,
            "Record whether the finding is refuted (refuted: bool, reason: string).",
            refutation_schema(),
            None,
        )
        .await;
    match v {
        Some(v) => v.get("refuted").and_then(Value::as_bool).unwrap_or(true),
        // No parseable vote — count as refuted for the majority tally (the paired refuter
        // still guards a genuine finding by NOT refuting it).
        None => true,
    }
}

// -- Apex judge --------------------------------------------------------------------------

const APEX_PROMPT: &str = "You are the apex judge. You are READ-ONLY and you NEVER generate \
    implementation content — you only adjudicate. Given the pre-assembled candidates, the \
    evidence, and the rubric, choose the single best candidate and justify it briefly. Call \
    `emit_verdict` with {chosen, rationale}. Do not propose a new candidate of your own.";

/// The apex judge's verdict + whether it had to fall back off Ultra (so the caller can
/// surface that the adjudication ran on a weaker tier).
pub struct ApexVerdict {
    pub verdict: Value,
    pub warned_tier_fallback: bool,
}

/// Run the apex judge over pre-assembled `candidates` + `evidence` against `rubric`. Runs at
/// the Ultra tier when reachable, else the session tier with an explicit warning chunk
/// already emitted. Read-only + verdict-JSON only (GBNF-forced, parse-and-repair fallback).
pub async fn apex_judge(
    jc: &JudgeContext,
    candidates: &str,
    evidence: &str,
    rubric: &str,
) -> ApexVerdict {
    let ultra = jc.llm.ultra_reachable();
    let tier = jc.max_available_tier("phán quyết cuối (apex judge)").await;
    let task = format!(
        "## Candidates\n{}\n\n## Evidence (quoted data)\n{}\n\n## Rubric\n{}",
        crate::tool_call::strip_tool_tags(candidates),
        crate::tool_call::strip_tool_tags(evidence),
        crate::tool_call::strip_tool_tags(rubric),
    );
    let verdict = jc
        .run_json_subturn(
            APEX_PROMPT,
            task,
            EMIT_VERDICT_TOOL,
            "Record the verdict (chosen: string, rationale: string).",
            verdict_schema(),
            tier,
        )
        .await
        .unwrap_or_else(|| json!({ "chosen": "", "rationale": "apex judge produced no parseable verdict" }));
    ApexVerdict { verdict, warned_tier_fallback: !ultra }
}

#[cfg(test)]
mod tests;
