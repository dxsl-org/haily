use haily_db::{queries::skills as db_skills, DbHandle};
use haily_kms::skills::TaskOutcome;
use haily_tools::ToolRegistry;
use std::sync::Arc;

use crate::tool_call;

/// VN/EN phrases a model uses when it is explicitly giving up on a task, as opposed
/// to merely mentioning a tool failure in passing while still delivering a partial
/// answer. Deliberately narrow (phase-08, F22) — this feeds `TaskOutcome::compute`'s
/// "final response signals inability" input, and a false positive here would
/// mislabel a legitimately completed turn as a failure, dragging down EMA confidence
/// for skills that had nothing to do with the (nonexistent) failure.
const INABILITY_PHRASES: &[&str] = &[
    "không thể",
    "không làm được",
    "xin lỗi, tôi không",
    "i cannot",
    "i can't",
    "i'm unable to",
    "i am unable to",
    "unable to complete",
];

/// Whether `response` (the turn's final, already tag-stripped text) reads as the
/// model giving up on the task entirely, rather than a normal answer.
fn signals_inability(response: &str) -> bool {
    let lower = response.to_lowercase();
    INABILITY_PHRASES.iter().any(|p| lower.contains(p))
}

/// Failed-call count from the same `Vec<serde_json::Value>` shape both `run_turn`
/// and `run_sub_turn` accumulate (`{"tool":..,"args":..,"ok":bool}` per call).
fn count_failed_calls(tool_call_log: &[serde_json::Value]) -> usize {
    tool_call_log.iter().filter(|e| e["ok"] == false).count()
}

/// Derive `(approval_requested, approval_denied)` for the whole turn by REPLAYING
/// `tool_call::dispatch`'s exact gating rules against the dispatched-call log, rather
/// than a bare `tool.risk_tier()` re-derivation (Harness Completion phase 5, H1 fix —
/// the prior version diverged from broker reality in two undocumented, opposite
/// directions; both are fixed here instead of merely documented, per the review):
///
/// 1. **M2 cap escalation** (`tool_call.rs`'s `RETIERED_DELETE_TOOLS` +
///    `MAX_AUTO_DELETES_PER_TURN`): a re-tiered delete (`task_delete`/`note_delete`/
///    `reminder_delete`) is `ReversibleWrite` on `tool.risk_tier()` — the OLD code
///    checked only that raw tier and so NEVER counted a cap-escalated delete as
///    "requested," even though `dispatch` genuinely raised an interactive prompt for
///    it (a false NEGATIVE, undercounting real approval requests). Fixed by replaying
///    the SAME escalation predicate `dispatch` evaluates: track a running delete
///    counter and escalate to `IrreversibleWrite` once it reaches the cap, exactly as
///    `tool_call::dispatch` does.
/// 2. **Auto-approve allowlist** (`ApprovalGate::is_auto_approved`): a genuinely
///    `IrreversibleWrite` tool on the allowlist never raises an interactive prompt —
///    the OLD code counted it as "requested" regardless (a false POSITIVE, overcounting
///    prompts that were never shown). Fixed by consulting the SAME `approval_gate`
///    `dispatch` consults before counting a call as requested.
///
/// Residual imprecision (documented, not silently trusted): the running delete
/// counter is reconstructed by walking THIS call's own `tool_call_log`, seeded from
/// `final_turn_deletes` minus this log's own successful-retiered-delete count — exact
/// for a single call site's log in isolation, but if a DELEGATED sub-turn's deletes
/// interleave with the parent L0 turn's deletes against the SAME shared
/// `ctx.turn_deletes` counter (cross-delegation cumulative cap, Harness Completion
/// phase 2), the seed can only be inferred, not observed per-call, for whichever side
/// runs second. This narrows (does not eliminate) the earlier blanket approximation:
/// within one call site's own log — the overwhelmingly common case, and the only case
/// for a non-delegating turn — this is now exact.
fn approval_stats(
    tool_call_log: &[serde_json::Value],
    tools: &ToolRegistry,
    approval_gate: &Arc<dyn haily_types::ApprovalGate>,
    final_turn_deletes: usize,
) -> (bool, bool) {
    let is_successful_retiered_delete = |entry: &serde_json::Value| {
        entry["tool"]
            .as_str()
            .map(|n| tool_call::RETIERED_DELETE_TOOLS.contains(&n))
            .unwrap_or(false)
            && entry["ok"] == true
    };
    let successful_retiered_in_log = tool_call_log
        .iter()
        .filter(|e| is_successful_retiered_delete(e))
        .count();
    // Reconstruct the counter's value BEFORE this log's own calls ran. Saturating: a
    // delegated sub-turn's log can never itself exceed the shared final count.
    let mut running_deletes = final_turn_deletes.saturating_sub(successful_retiered_in_log);

    let mut requested = false;
    let mut denied = false;
    for entry in tool_call_log {
        let Some(name) = entry["tool"].as_str() else {
            continue;
        };
        let Some(tool) = tools.get(name) else {
            continue;
        };
        let args: serde_json::Value = entry["args"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Null);
        let tier = tool.risk_tier(&args);
        let is_retiered = tool_call::RETIERED_DELETE_TOOLS.contains(&name);
        let effective_tier = if is_retiered
            && tier == haily_tools::RiskTier::ReversibleWrite
            && running_deletes >= haily_tools::MAX_AUTO_DELETES_PER_TURN
        {
            haily_tools::RiskTier::IrreversibleWrite
        } else {
            tier
        };

        if effective_tier == haily_tools::RiskTier::IrreversibleWrite
            && !approval_gate.is_auto_approved(name)
        {
            requested = true;
            if entry["ok"] == false {
                denied = true;
            }
        }

        if is_retiered && entry["ok"] == true {
            running_deletes += 1;
        }
    }
    (requested, denied)
}

/// Pair a `(prompt, completion)` token usage sample into a well-formed, all-or-nothing telemetry
/// value (Sub-Agent + Skill Architecture phase 8, FMA-m5 C2 pairing contract).
///
/// An attempt's usage is recorded PER ATTEMPT with its resolved backend, never rolled up across
/// backends: a local backend surfaces no usage (both `None`), a cloud one surfaces both. A MIXED
/// pair (one `Some`, one `None`) is corrupt — it would let a per-stage rollup average a real
/// token count against a fabricated zero and poison the GateResult EMA — so this collapses any
/// mixed pair to `(None, None)`. Only a genuinely complete pair passes through as `Some`/`Some`.
pub(crate) fn pair_usage(
    prompt: Option<i64>,
    completion: Option<i64>,
) -> (Option<i64>, Option<i64>) {
    match (prompt, completion) {
        (Some(p), Some(c)) => (Some(p), Some(c)),
        // Missing either half → emit None/None explicitly (never a mixed Some/None).
        _ => (None, None),
    }
}

/// Whether `tool_call_log` contains a `feedback_react` call this SAME turn whose
/// `reaction` is `negative` or `correction` — the m2-review (M2) same-turn
/// corroborator for `repeat_request`: an explicit user reaction within this turn's
/// own tool-call log (not a cross-turn join, which stays `feedback.rs`'s
/// attribution-gated `downgrade_prior_trace` path) is at least as strong evidence as
/// the `Partial`-outcome corroborator.
fn has_explicit_negative_feedback_this_turn(tool_call_log: &[serde_json::Value]) -> bool {
    tool_call_log.iter().any(|entry| {
        entry["tool"].as_str() == Some("feedback_react")
            && entry["args"]
                .as_str()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .and_then(|v| v["reaction"].as_str().map(str::to_string))
                .is_some_and(|r| r == "negative" || r == "correction")
    })
}

/// The handful of fields `run_turn` and `run_sub_turn` cannot share verbatim when
/// calling `record_outcome_and_update_skill` — everything else about the label/
/// telemetry/EMA wiring (outcome computation, undo/repeat lookups, `derive_label`,
/// `TraceMetrics` assembly, `insert_trace`, the anti-reinforcement `unknown` guard)
/// is byte-for-byte identical between the two call sites, so it lives in that one
/// shared helper instead of two near-duplicate blocks.
pub(super) struct OutcomeMetricsInput<'a> {
    pub(super) model_tier: Option<&'a str>,
    pub(super) prompt_tokens: Option<i64>,
    pub(super) completion_tokens: Option<i64>,
    pub(super) delegate_overhead_ms: Option<i64>,
    /// Distinguishes the two call sites' `tracing::warn!` messages on a failed
    /// `update_skill_confidence` — kept as a caller-supplied literal rather than
    /// inferred, so the log text stays exactly what each site logged before this
    /// helper existed.
    pub(super) confidence_update_failure_msg: &'static str,
    /// M3 review fix: whether THIS call site is allowed to drive
    /// `update_skill_confidence` at all. A delegated sub-turn (`run_sub_turn`) always
    /// STILL inserts its own trace (telemetry — `delegate_overhead_ms`, tool counts,
    /// etc. — keeps its per-sub-turn granularity, which is genuinely useful data),
    /// but `false` here skips the EMA nudge, because the PARENT L0 turn's own
    /// `record_outcome_and_update_skill` call ALSO runs (once, always, at the end of
    /// `run_turn`) and would otherwise double-apply the EMA for one user-visible
    /// action routed through a delegate — silently doubling the effective learning
    /// rate for every delegated task versus a non-delegated one. Only `run_turn`
    /// (L0, depth 0) sets this `true`; `run_sub_turn` (any depth ≥ 1) sets it `false`.
    pub(super) owns_learning: bool,
    /// H1 review fix: the SAME seam handles `tool_call::dispatch` consults, threaded
    /// through so `approval_stats` can replay dispatch's exact gating (auto-approve
    /// allowlist + M2 per-turn delete-cap escalation) instead of re-deriving a bare
    /// `RiskTier`. See `approval_stats`'s doc comment for the two divergences this
    /// closes.
    pub(super) approval_gate: &'a Arc<dyn haily_types::ApprovalGate>,
    /// Final (end-of-call) value of the turn's shared destructive-delete counter —
    /// `approval_stats` reconstructs each call's PRE-dispatch counter value by
    /// replaying this log against it. See `approval_stats`'s doc comment for the
    /// residual cross-delegation imprecision this cannot fully resolve.
    pub(super) final_turn_deletes: usize,
    /// The turn's minted id (`run_turn`'s own UUID, or the PARENT turn's id reused by a
    /// delegated `run_sub_turn` — see `ToolContext::turn_id`'s doc) — the R2 join key
    /// threaded into `TraceMetrics.turn_id` (Auto Model Routing R1 phase 2/4) so a trace
    /// row can be joined against this same turn's `routing_decisions`/`action_journal` rows.
    pub(super) turn_id: &'a str,
}

/// Shared tail of `run_turn`/`run_sub_turn`: compute the turn's outcome label from
/// undo/repeat/tool-error signals, persist the trace with its telemetry, and —
/// only when a real signal fired AND this call site owns learning — nudge a matching
/// skill's EMA confidence.
///
/// SAFETY (anti-reinforcement invariant): `label.is_unknown()` must short-circuit
/// BOTH the label persisted (`None`, not `Some("unknown")`) and the EMA update
/// (skipped entirely, never defaulted to a neutral reward) — see the doc comments
/// on `TraceMetrics::label_source` and the `outcome_signal_tests` module below.
#[allow(clippy::too_many_arguments)]
pub(super) async fn record_outcome_and_update_skill(
    db: &DbHandle,
    session_id: &str,
    task_description: &str,
    tool_call_log: &[serde_json::Value],
    tools: &ToolRegistry,
    final_response: &str,
    elapsed_ms: i64,
    input: OutcomeMetricsInput<'_>,
) {
    let tool_calls_json = serde_json::to_string(tool_call_log).unwrap_or_default();
    let failed_calls = count_failed_calls(tool_call_log);
    let outcome = TaskOutcome::compute(
        signals_inability(final_response),
        failed_calls,
        tool_call_log.len(),
    );

    let trace_created_at = chrono::Utc::now().to_rfc3339();
    let undo_within_5min = db_skills::undo_within_n_min(
        db,
        session_id,
        &trace_created_at,
        haily_kms::skills::UNDO_WINDOW_MINUTES,
    )
    .await
    .unwrap_or(false);
    // Repeat-request detection (researcher-03 §1): checked BEFORE this turn's own
    // trace is inserted, so "previous" genuinely means the prior turn, not this one
    // compared against itself.
    let is_repeat = match db_skills::most_recent_trace(db, session_id).await {
        Ok(Some(prev)) => {
            haily_kms::skills::is_repeat_request(&prev.task_description, task_description)
        }
        _ => false,
    };
    // M2 review fix: an uncorroborated repeat (a clean turn, no other negative
    // indicator) must not read as a failure signal — see `derive_label`'s doc
    // comment. `Partial` (some-but-not-most tool calls failed) or an explicit
    // negative/correction `feedback_react` call THIS turn both corroborate; a
    // `Failure` outcome already wins its own branch in `derive_label` before
    // `is_repeat_request` is even considered, so it is deliberately NOT included
    // here (would never be reachable).
    let has_corroborating_negative_signal =
        outcome == TaskOutcome::Partial || has_explicit_negative_feedback_this_turn(tool_call_log);
    let label = haily_kms::skills::derive_label(
        outcome,
        undo_within_5min,
        is_repeat,
        has_corroborating_negative_signal,
    );
    let (approval_requested, approval_denied) = approval_stats(
        tool_call_log,
        tools,
        input.approval_gate,
        input.final_turn_deletes,
    );

    let metrics = db_skills::TraceMetrics {
        model_tier: input.model_tier,
        prompt_tokens: input.prompt_tokens,
        completion_tokens: input.completion_tokens,
        tool_call_count: Some(tool_call_log.len() as i64),
        approval_requested: Some(approval_requested),
        approval_denied: Some(approval_denied),
        undo_within_5min: Some(undo_within_5min),
        // `label_source IS NULL` is the DB-level contract for "no signal fired"
        // (see `TaskTrace::label_source`'s doc comment) — an `unknown` label must
        // persist as NULL, not the literal string "unknown", so a rollup/query never
        // has to special-case a magic string alongside NULL.
        label_source: if label.is_unknown() {
            None
        } else {
            Some(label.source.as_str())
        },
        label_confidence: if label.is_unknown() {
            None
        } else {
            Some(label.confidence)
        },
        delegate_overhead_ms: input.delegate_overhead_ms,
        // Auto Model Routing R1 phase 4: closes the cross-phase gap phase 2 flagged —
        // `TraceMetrics.turn_id` now carries the real minted turn id from both call sites.
        turn_id: Some(input.turn_id),
    };

    let _ = db_skills::insert_trace(
        db,
        session_id,
        task_description,
        &tool_calls_json,
        outcome.as_str(),
        Some(elapsed_ms),
        metrics,
    )
    .await;

    // SAFETY (anti-reinforcement invariant): `unknown` NEVER drives the EMA — skip
    // the call entirely rather than defaulting to a neutral reward (memory
    // 2026-06-21 project-memory-anti-reinforcement-plan; researcher-03 §2.2). Only
    // reachable when a real signal fired, this turn's task matched an active skill
    // closely enough (`find_matching_skill`) — most turns correspond to no
    // synthesized skill yet, and that must not be forced into a false match — AND
    // (M3 review fix) this call site owns learning: a delegated sub-turn's trace is
    // still inserted above (telemetry value stands) but never drives the EMA itself,
    // since the parent L0 turn's OWN `record_outcome_and_update_skill` call already
    // does so once for the whole user-visible action.
    if !label.is_unknown() && input.owns_learning {
        if let Ok(active) = db_skills::active_skills(db).await {
            if let Some(skill) = haily_kms::skills::find_matching_skill(task_description, &active) {
                let reward = outcome.ema_reward() * label.confidence;
                if let Err(e) =
                    haily_kms::skills::update_skill_confidence(db, &skill.id, reward).await
                {
                    tracing::warn!(skill_id = %skill.id, error = %e, "{}", input.confidence_update_failure_msg);
                }
            }
        }
    }
}

#[cfg(test)]
mod approval_stats_tests {
    //! Harness Completion phase 5, H1 review fix — `approval_stats` must replay
    //! `tool_call::dispatch`'s exact gating (M2 cap escalation + auto-approve
    //! allowlist) rather than a bare `RiskTier` re-derivation. These tests prove the
    //! two divergences the review identified are now closed, using synthetic
    //! `tool_call_log` entries (no real dispatch needed — `approval_stats` is a pure
    //! function of the log + registry + gate + counter).
    use super::*;
    use crate::approval::ApprovalBroker;
    use anyhow::Result;
    use async_trait::async_trait;
    use haily_tools::{RiskTier, Tool, ToolContext, ToolRegistry};

    /// Stand-in for a re-tiered delete tool (`task_delete` — in
    /// `RETIERED_DELETE_TOOLS`), constant `ReversibleWrite` on `risk_tier()` exactly
    /// like the real `TaskDeleteTool`.
    struct RetieredDeleteToolFixture;

    #[async_trait]
    impl Tool for RetieredDeleteToolFixture {
        fn name(&self) -> &str {
            "task_delete"
        }
        fn description(&self) -> &str {
            "fixture"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn risk_tier(&self, _args: &serde_json::Value) -> RiskTier {
            RiskTier::ReversibleWrite
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<String> {
            Ok("deleted".to_string())
        }
    }

    /// A tool that is genuinely `IrreversibleWrite` on its own merits (not a
    /// cap-escalated `ReversibleWrite`) — e.g. stands in for `memory_forget`.
    struct GenuineIrreversibleToolFixture;

    #[async_trait]
    impl Tool for GenuineIrreversibleToolFixture {
        fn name(&self) -> &str {
            "memory_forget"
        }
        fn description(&self) -> &str {
            "fixture"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn risk_tier(&self, _args: &serde_json::Value) -> RiskTier {
            RiskTier::IrreversibleWrite
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<String> {
            Ok("forgotten".to_string())
        }
    }

    fn log_entry(tool: &str, ok: bool) -> serde_json::Value {
        serde_json::json!({ "tool": tool, "args": "{}", "ok": ok })
    }

    /// H1 case 1 (the OLD false-negative): a cap-escalated `task_delete` call — the
    /// tool's own `risk_tier()` is `ReversibleWrite`, but `final_turn_deletes` is
    /// already at the cap when this call ran, so `dispatch` genuinely raised an
    /// interactive approval prompt for it. The fix must count this as
    /// `approval_requested = true`, which the OLD bare-`risk_tier()` check could
    /// never do (it only ever saw `ReversibleWrite`, never the escalation).
    #[tokio::test]
    async fn cap_escalated_retiered_delete_is_counted_as_requested() {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(RetieredDeleteToolFixture));
        let gate: Arc<dyn haily_types::ApprovalGate> = Arc::new(ApprovalBroker::new());

        // The counter was ALREADY at the cap before this (the only) call in the log
        // ran, so dispatch would have escalated it to IrreversibleWrite and raised a
        // prompt; since it succeeded (ok:true), the counter's FINAL value is one
        // past the cap (mirrors `tool_call.rs`'s own
        // `cap_escalation_approved_still_executes_and_increments_counter` behavior:
        // an approved escalated delete still executes and still increments).
        let log = vec![log_entry("task_delete", true)];
        let (requested, denied) = approval_stats(
            &log,
            &tools,
            &gate,
            haily_tools::MAX_AUTO_DELETES_PER_TURN + 1,
        );

        assert!(
            requested,
            "a cap-escalated re-tiered delete must be counted as approval_requested \
             (H1 fix — the old bare-RiskTier check always missed this)"
        );
        assert!(
            !denied,
            "this call succeeded (ok:true), so it must not be counted as denied"
        );
    }

    /// Mirror of the above: a cap-escalated delete that was DENIED must count as
    /// both requested AND denied.
    #[tokio::test]
    async fn cap_escalated_retiered_delete_denied_is_counted_as_denied() {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(RetieredDeleteToolFixture));
        let gate: Arc<dyn haily_types::ApprovalGate> = Arc::new(ApprovalBroker::new());

        let log = vec![log_entry("task_delete", false)];
        let (requested, denied) =
            approval_stats(&log, &tools, &gate, haily_tools::MAX_AUTO_DELETES_PER_TURN);

        assert!(requested);
        assert!(
            denied,
            "a denied (ok:false) escalated delete must be counted as denied"
        );
    }

    /// A re-tiered delete UNDER the cap must NOT be counted as requested — it
    /// auto-runs with no interactive prompt, exactly like the real dispatch path.
    #[tokio::test]
    async fn under_cap_retiered_delete_is_not_counted_as_requested() {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(RetieredDeleteToolFixture));
        let gate: Arc<dyn haily_types::ApprovalGate> = Arc::new(ApprovalBroker::new());

        // final_turn_deletes = 0, well under the cap.
        let log = vec![log_entry("task_delete", true)];
        let (requested, _denied) = approval_stats(&log, &tools, &gate, 0);

        assert!(
            !requested,
            "a re-tiered delete under the cap auto-runs with no prompt — must not count as requested"
        );
    }

    /// H1 case 2 (the OLD false-positive): a genuinely `IrreversibleWrite` tool on
    /// the auto-approve allowlist never raises an interactive prompt in real
    /// dispatch — the fix must NOT count it as requested. The old bare-`RiskTier`
    /// check counted every `IrreversibleWrite` call as requested regardless of the
    /// allowlist, overcounting prompts that were never shown.
    #[tokio::test]
    async fn allowlisted_irreversible_write_is_not_counted_as_requested() {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(GenuineIrreversibleToolFixture));
        let gate: Arc<dyn haily_types::ApprovalGate> = Arc::new(ApprovalBroker::with_auto_approve(
            ["memory_forget".to_string()].into_iter().collect(),
        ));

        let log = vec![log_entry("memory_forget", true)];
        let (requested, denied) = approval_stats(&log, &tools, &gate, 0);

        assert!(
            !requested,
            "an allowlisted IrreversibleWrite tool never raises a prompt in real \
             dispatch — must not be counted as requested (H1 fix)"
        );
        assert!(!denied);
    }

    /// A genuinely `IrreversibleWrite` tool NOT on the allowlist DOES raise a real
    /// prompt — must be counted as requested (the baseline case both the old and new
    /// implementations should agree on).
    #[tokio::test]
    async fn non_allowlisted_irreversible_write_is_counted_as_requested() {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(GenuineIrreversibleToolFixture));
        let gate: Arc<dyn haily_types::ApprovalGate> = Arc::new(ApprovalBroker::new());

        let log = vec![log_entry("memory_forget", true)];
        let (requested, _denied) = approval_stats(&log, &tools, &gate, 0);

        assert!(
            requested,
            "a non-allowlisted IrreversibleWrite call must count as requested"
        );
    }
}

#[cfg(test)]
mod pure_helper_tests {
    //! Pure-function tests for `signals_inability`/`count_failed_calls`. Split out of
    //! the pre-refactor monolith's `outcome_tests` module, which mixed these with
    //! `run_sub_turn`-driving integration tests — those moved to `agent::sub_turn`
    //! alongside the code they exercise (see that module's `outcome_tests`).
    use super::*;

    #[test]
    fn signals_inability_detects_vietnamese_and_english_giveup_phrases() {
        assert!(signals_inability("Xin lỗi, tôi không thể giúp việc này."));
        assert!(signals_inability("I'm unable to complete this task."));
    }

    #[test]
    fn signals_inability_false_on_a_normal_answer() {
        assert!(!signals_inability("Đây là kết quả bạn cần: 42."));
        assert!(!signals_inability("Here is the answer you asked for."));
    }

    /// FMA-m5 C2 pairing contract: a MIXED usage pair (one half missing) must collapse to
    /// None/None, never a Some/None that a rollup could average against a real count.
    #[test]
    fn pair_usage_collapses_a_mixed_pair_to_none_none() {
        assert_eq!(pair_usage(Some(100), Some(50)), (Some(100), Some(50)));
        assert_eq!(pair_usage(None, None), (None, None));
        assert_eq!(
            pair_usage(Some(100), None),
            (None, None),
            "prompt-only must not leak through"
        );
        assert_eq!(
            pair_usage(None, Some(50)),
            (None, None),
            "completion-only must not leak through"
        );
    }

    #[test]
    fn count_failed_calls_counts_only_ok_false_entries() {
        let log = serde_json::json!([
            {"tool": "a", "args": "{}", "ok": true},
            {"tool": "b", "args": "{}", "ok": false},
            {"tool": "c", "args": "{}", "ok": false},
        ]);
        let log = log.as_array().unwrap().clone();
        assert_eq!(count_failed_calls(&log), 2);
    }
}
