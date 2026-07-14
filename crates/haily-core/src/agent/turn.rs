use anyhow::Result;
use haily_db::{
    queries::{routing_decisions, sessions, work_items},
    DbHandle,
};
use haily_kms::KmsHandle;
use haily_llm::{
    CompletionRequest, Egress, EscalationPolicy, LlmRouter, Message, Role, RouterSnapshot, Tier,
};
use haily_tools::{ToolContext, ToolRegistry};
use haily_types::{Request, ResponseChunk};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

use crate::approval::ApprovalBroker;
use crate::routing::{self, TierDecision};
use crate::{budget, context, feedback_parser, tool_call};
use super::outcome::{record_outcome_and_update_skill, OutcomeMetricsInput};
use super::stream::stream_llm_response;

fn estimate_tokens(s: &str) -> i64 {
    (s.len() / 4) as i64
}

/// The L0 transparency badge (Auto Model Routing R1): "tier · model" when a tier was
/// actually selected for this turn's last call, else just the session-default model name —
/// transparency even when routing didn't act. Pure and unit-testable, mirroring
/// `render_tool_result_line`'s split-out-for-testing pattern in `haily-io::cli`.
fn build_turn_meta_badge(snapshot: &RouterSnapshot, tier: Option<Tier>) -> String {
    match tier {
        Some(t) => format!(
            "{} · {}",
            routing::tier_label(t),
            snapshot.model_for_tier(Some(t))
        ),
        None => snapshot.model_for_tier(None).to_string(),
    }
}

#[cfg(test)]
mod turn_meta_badge_tests {
    use super::*;
    use haily_llm::{LlmConfig, TierModels};

    /// `Some(tier)` with a configured tier-model override renders "tier · model".
    #[tokio::test]
    async fn some_tier_with_override_renders_tier_and_model() {
        let llm = LlmRouter::init(LlmConfig {
            cloud_api_keys: vec!["k".to_string()],
            cloud_base_url: "http://127.0.0.1:1".to_string(),
            cloud_model: "default-model".to_string(),
            tier_models: TierModels {
                thinking: Some("big-model".to_string()),
                ..TierModels::default()
            },
            ..LlmConfig::default()
        })
        .await;
        let badge = build_turn_meta_badge(&llm.snapshot(), Some(Tier::Thinking));
        assert_eq!(badge, "thinking · big-model");
    }

    /// `Some(tier)` with NO override for that tier falls back to the session-default
    /// model name, still prefixed with the tier label (mirrors `complete_tiered`'s own
    /// no-override fallback).
    #[tokio::test]
    async fn some_tier_without_override_falls_back_to_default_model() {
        let llm = LlmRouter::init(LlmConfig {
            cloud_api_keys: vec!["k".to_string()],
            cloud_base_url: "http://127.0.0.1:1".to_string(),
            cloud_model: "default-model".to_string(),
            ..LlmConfig::default()
        })
        .await;
        let badge = build_turn_meta_badge(&llm.snapshot(), Some(Tier::Fast));
        assert_eq!(badge, "fast · default-model");
    }

    /// `None` tier shows the bare session-default model name — no `·` separator, since
    /// there is no tier to show (transparency even when routing didn't act).
    #[tokio::test]
    async fn none_tier_shows_bare_default_model_name() {
        let llm = LlmRouter::init(LlmConfig {
            cloud_api_keys: vec!["k".to_string()],
            cloud_base_url: "http://127.0.0.1:1".to_string(),
            cloud_model: "default-model".to_string(),
            ..LlmConfig::default()
        })
        .await;
        let badge = build_turn_meta_badge(&llm.snapshot(), None);
        assert_eq!(badge, "default-model");
    }
}

/// Shared runtime handles for a full turn — grouped so `run_turn` stays within a
/// sane arity (mirrors `SubTurnRequest`'s reasoning for sub-turns). These are the
/// same handles `Orchestrator` already holds as fields; `process` just forwards them.
pub struct TurnRuntime {
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    pub llm: Arc<LlmRouter>,
    pub tools: Arc<ToolRegistry>,
    /// Phase 3 kill switch (C8): `safety.disable_writes`, shared from the Orchestrator so
    /// the L0 turn and every sub-turn it spawns observe the same live-toggleable flag.
    pub kill: Arc<AtomicBool>,
    /// Auto Model Routing R1 (phase 4) kill switch: `llm.routing_enabled`, mirroring `kill`
    /// exactly — a shared, live-flippable `Arc<AtomicBool>` from the Orchestrator. `false`
    /// makes this turn behave identically to the pre-phase-4 turn (tier always `None`, zero
    /// `routing_decisions` rows, no escalation rescue attempted).
    pub routing_enabled: Arc<AtomicBool>,
    /// View Engine Phase A (phase 3): the SAME `Arc<ViewStore>` the Orchestrator holds for
    /// its whole lifetime (`Orchestrator::view_store()`), not a fresh turn-scoped store —
    /// so a `present_view` insert here and the `get_view` Tauri command's read observe one
    /// shared snapshot set.
    pub view_store: Arc<crate::view::ViewStore>,
}

/// Full agent turn. Called once per incoming Request.
///
/// `broker` gates `RiskTier::IrreversibleWrite` tool calls at depth 0; `cancel` is the
/// turn's cancellation token — firing it (shutdown) denies any pending approval
/// immediately instead of holding up the drain for up to the 120s approval timeout.
#[instrument(skip_all, fields(session = %req.session_id))]
pub async fn run_turn(
    req: &Request,
    runtime: TurnRuntime,
    tx: mpsc::Sender<ResponseChunk>,
    broker: &Arc<ApprovalBroker>,
    cancel: &CancellationToken,
) -> Result<()> {
    let TurnRuntime {
        db,
        kms,
        llm,
        tools,
        kill,
        routing_enabled,
        view_store,
    } = runtime;
    let session_id = req.session_id.to_string();
    let turn_start = std::time::Instant::now();

    // Ensure session exists in DB, created under req.session_id so that
    // work_items.session_id (FK to sessions.id) resolves for this turn.
    if sessions::get_session(&db, &session_id).await?.is_none() {
        sessions::create_session(&db, &session_id, &req.adapter_id, req.user_ref.as_deref())
            .await?;
    } else {
        sessions::touch_session(&db, &session_id).await?;
    }

    // Detect and persist feedback signal before inserting user message.
    //
    // SECURITY (m2): `req.message` — the `Request::message` field — is the ONLY text
    // this function ever passes to `detect_feedback`. It is the genuine incoming user
    // message, never a tool result or fetched/pasted document body: those flow into
    // the LLM's own `messages` history as `<tool_result>` blocks below, and are never
    // re-read as `req.message` by this or any later turn. A phrase like "no, that's
    // wrong" embedded in a pasted document or a tool's output therefore cannot reach
    // `detect_feedback` through this call site — it would have to appear in the text
    // the user themselves typed/sent this turn. `is_explicit = false`: this is a
    // pattern-matched guess, capped below an explicit `feedback_react` tool call's
    // confidence (see `apply_feedback_signal`'s doc comment).
    if let Some(signal) = feedback_parser::detect_feedback(&req.message) {
        let _ = feedback_parser::apply_feedback_signal(&signal, &db, &session_id, false).await;
    }

    // Phase 7 depth: a VN/EN depth phrase in the GENUINE user message (`req.message` — the
    // SAME source-guarded input feedback detection uses above, never tool/pasted content)
    // OVERRIDES the request's toggle-set depth for this turn; absent a phrase the toggle
    // value stands. The harness NEVER escalates to Deep on its own — this is either an
    // explicit toggle or an explicit phrase. Threaded onto `ToolContext.depth_mode` below so
    // every delegation inherits it (a researcher/writer sub-agent picks up its depth
    // playbook variant); the LLM can never forge it.
    let effective_depth = crate::depth::effective_depth(req.depth, &req.message);

    sessions::insert_message(&db, &session_id, "user", &req.message, None).await?;
    info!(session = session_id, "processing user message");

    let context_window = llm.context_window();
    let token_budget = budget::TokenBudget::new(context_window);
    let (mut messages, _ctx) =
        context::build_messages(&kms, &db, &tools, &session_id, &req.message, context_window)
            .await?;

    // Minted ONCE per turn (never from LLM/task text) — every tool call this turn, and
    // every sub-turn `delegate.rs` spawns from it, shares this id/counter so the whole
    // turn's writes group under one `undo_turn` and one M2 destructive-op cap.
    let turn_id = uuid::Uuid::new_v4();
    let turn_id_str = turn_id.to_string();
    let turn_deletes = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Auto Model Routing R1 (phase 4): the tier decision is computed EXACTLY ONCE here —
    // `current_tier` is reused by every loop iteration below (the anti-flap invariant) and
    // only ever mutated by `open_stream_with_escalation`'s own rescue step, never
    // recomputed from `select_tier` again within this turn. `routing_active=false` makes
    // `decision` the pure `Default` (tier=None, source=default) — identical model
    // selection to a pre-phase-4 turn, and (checked again at turn end) zero
    // `routing_decisions` rows.
    let routing_active = routing_enabled.load(Ordering::Acquire);
    // Count of PRIOR user messages actually surviving this turn's token-budget fit — a
    // trusted-origin, privacy-safe COUNT (never assembled history text; see
    // `routing::RouteCtx`'s injection-invariant doc). `messages` already includes the
    // CURRENT turn's own user message (`context::build_messages` always appends it and
    // pins it), so it is excluded via `saturating_sub(1)`.
    let history_user_msgs = messages
        .iter()
        .filter(|m| m.role == Role::User)
        .count()
        .saturating_sub(1);
    let route_ctx = routing::RouteCtx {
        depth: effective_depth,
        history_user_msgs,
        // Mirrors the existing `deny_remote_deep` remote-origin check (mobile/server.rs,
        // `adapter_id: "mobile"` is the only remote-transport literal any adapter sets).
        remote_origin: routing::is_remote_adapter(&req.adapter_id),
    };
    let decision = if routing_active {
        routing::select_tier(&req.message, route_ctx, llm.cost_quality())
    } else {
        TierDecision::default()
    };
    let mut current_tier = decision.tier;
    // Chat escalation policy (LOCKED, phase 4 spec): a single dead-end rescue on a
    // pre-first-token stream failure — strictly better than today's hard-fail. Gated on
    // `routing_active` so the kill switch's documented guarantee holds literally: with
    // routing OFF, a stream failure reproduces the legacy hard-fail even when a tier
    // override is configured (previously the no-op guard only caught the no-override
    // case, leaving an untested gap where OFF could still switch models on failure).
    let escalation_policy = EscalationPolicy {
        failures_before_escalation: 1,
        max_tier: Tier::Thinking,
        enabled: routing_active,
    };
    // Egress derivation (LOCKED): llama primary ⇒ LocalOnly (escalation never silently
    // leaves the machine the user started local on); cloud primary ⇒ AllowCloud. An
    // optional `llm.escalation.egress` preference overrides either derivation; read fresh
    // each turn (not cached at boot) so an operator change takes effect immediately.
    let egress_override = haily_db::queries::meta::get_preference(&db, "llm.escalation.egress")
        .await
        .ok()
        .flatten()
        .and_then(|v| routing::parse_egress_override(&v));
    let egress = egress_override.unwrap_or(if llm.provider_name() == "llama.cpp" {
        Egress::LocalOnly
    } else {
        Egress::AllowCloud
    });

    let tool_ctx = ToolContext {
        db: db.clone(),
        kms: kms.clone(),
        session_id: req.session_id,
        turn_id,
        depth: 0,
        // L0 has no single domain — `origin` renders as bare `"L0"`.
        domain: None,
        // Real L0 broker-as-gate/tx/cancel — `ApprovalBroker` also implements
        // `ApprovalGate` (approval.rs), so this is the SAME broker `dispatch` below
        // consults, not a parallel authorization path.
        approval_gate: Arc::clone(broker) as Arc<dyn haily_types::ApprovalGate>,
        approval_tx: tx.clone(),
        cancel: cancel.clone(),
        turn_deletes: Arc::clone(&turn_deletes),
        // Reset at the top of THIS turn's context; `tool_call::dispatch` additionally
        // resets it before every individual tool call within the turn (M4 no-bleed).
        last_journal_id: Arc::new(std::sync::Mutex::new(None)),
        // An L0 chat turn is not a pipeline run — only the P4b runner sets this.
        run_id: None,
        depth_mode: effective_depth,
        // View Engine Phase A (phase 3): the Orchestrator's ONE shared store (see
        // `TurnRuntime::view_store` doc) — a `present_view` insert here is visible to the
        // `get_view` Tauri command, which reads the SAME store off the Orchestrator handle.
        view_sink: Arc::clone(&view_store) as Arc<dyn haily_types::ViewSink>,
    };

    let mut guard = tool_call::LoopGuard::new();
    let mut tool_call_log: Vec<serde_json::Value> = Vec::new();

    // WorkItem tracking: lazily created on first tool call.
    // Simple Q&A turns (no tool calls) produce no WorkItem row.
    let mut work_item_id: Option<String> = None;
    let mut tool_index: usize = 0;

    // Pinned tail length: starts at 1 (just the user message `build_messages` already
    // appended) and grows by 2 (assistant tool-call + `<tool_result>`) per loop
    // iteration — everything from the user message onward must never be trimmed
    // (see `budget.rs`'s pinning rule), only the prior-turn history before it.
    let mut pinned_tail_len: usize = 1;

    // Whether the turn's final text still needs to reach the user via one more
    // `tx.send` at the bottom of this function, or was already fully delivered as
    // live increments by `stream_llm_response` during the loop above. The common
    // path (plain-text final answer, no tool call) streamed every safe byte already
    // — resending the whole string would duplicate it in the transcript. The
    // loop-guard's Vietnamese fallback message, by contrast, is fresh text that was
    // never streamed and DOES need this final send.
    let mut final_text_already_streamed = false;

    // C2 (Phase 8): the LAST LLM call's token counts, matching `final_response`
    // (also only ever the last iteration's text) — see the loop body's comment for
    // why only the final iteration's counts are kept, and `stream_llm_response`'s doc
    // comment for the llama-vs-cloud provenance contract these two must stay
    // gated on together (`Some`/`Some` or `None`/`None`, never mixed).
    let mut turn_prompt_tokens: Option<i64> = None;
    let mut turn_completion_tokens: Option<i64> = None;

    // Capture the loop result without propagating `?` immediately so the
    // WorkItem finalization block below always runs — even when LLM calls fail
    // mid-turn after the WorkItem has already been created.
    let loop_result: Result<String> = 'turn: {
        loop {
            // Re-fit before every LLM call (cheap — estimates only): a prior
            // iteration's `<tool_result>` may have grown the pinned tail enough that
            // history needs re-trimming to stay within budget, and this must happen
            // every iteration, not just once at turn start.
            messages = token_budget.refit(&messages, pinned_tail_len);

            let llm_req = CompletionRequest::simple(messages.clone()).with_cancel(cancel.clone());
            let mut stream = match routing::open_stream_with_escalation(
                &llm,
                &mut current_tier,
                llm_req,
                &escalation_policy,
                egress,
                cancel,
            )
            .await
            {
                Ok(rx) => rx,
                Err(e) => break 'turn Err(e),
            };
            // C2 (Phase 8): `prompt_tokens` is `StreamChunk::Done`'s own llama-vs-cloud
            // provenance signal (see `stream_llm_response`'s doc comment) — captured
            // per LLM call so the LAST call's counts (the one whose response becomes
            // `final_response`) can be persisted below. A turn with tool calls makes
            // several LLM calls in this loop; only the final iteration's counts are
            // kept, matching `final_response` itself (which is also only the last
            // iteration's text).
            let (response, total_tokens, prompt_tokens) =
                match stream_llm_response(&mut stream, &tx, cancel).await {
                    Ok(r) => r,
                    Err(e) => break 'turn Err(e),
                };
            // Only trust `total_tokens` as a real completion-token count when
            // `prompt_tokens` is `Some` (i.e. the llama.cpp backend served this call —
            // see the contract on `stream_llm_response`). Cloud calls leave both
            // `None`, preserving the NULL-honesty invariant `outcome_signal_tests`
            // asserts. Overwritten every iteration so only the LAST call's counts
            // survive to the `record_outcome_and_update_skill` call after the loop.
            (turn_prompt_tokens, turn_completion_tokens) = match prompt_tokens {
                Some(p) => (Some(p as i64), Some(total_tokens as i64)),
                None => (None, None),
            };

            if let Some((tool_name, args)) = tool_call::parse_tool_call(&response) {
                messages.push(Message {
                    role: Role::Assistant,
                    content: response.clone(),
                });

                // Guard BEFORE dispatch: a tripped guard ends the turn with the
                // model's own text (or a fallback) rather than feeding the error
                // back. L0 has no outer timeout, so feeding it back would let a
                // looping model spin unbounded while holding the WorkItem.
                if let Err(e) = guard.check(&tool_name, &args) {
                    tracing::warn!(error = %e, "loop guard tripped — ending turn");
                    let text = tool_call::strip_tool_markup(&response);
                    break 'turn Ok(if text.is_empty() {
                        "Tôi gặp vòng lặp khi xử lý yêu cầu này. Bạn thử diễn đạt lại giúp mình nhé.".to_string()
                    } else {
                        text
                    });
                }

                // Lazy WorkItem creation: only on the first tool call of this turn.
                if work_item_id.is_none() {
                    if let Ok(wi) = work_items::create(&db, &session_id, &req.message).await {
                        let _ = work_items::start(&db, &wi.id).await;
                        work_item_id = Some(wi.id);
                    }
                }

                let (result, tool_ok) =
                    tool_call::dispatch(&tool_name, args.clone(), &tools, &tool_ctx, &kill)
                        .await
                        .unwrap_or_else(|e| (format!("Error: {e:#}"), false));

                tool_call_log.push(serde_json::json!({
                    "tool": &tool_name,
                    "args": args.to_string(),
                    "ok":   tool_ok
                }));

                // Neutralize tool-protocol tags in the (possibly untrusted) result
                // before feeding it back — defuses second-order prompt injection.
                let safe_result = tool_call::strip_tool_tags(&result);
                let result_msg = format!(
                    "<tool_result>{{\"tool\":\"{tool_name}\",\"result\":{},\"ok\":{}}}</tool_result>",
                    serde_json::Value::String(safe_result),
                    tool_ok
                );
                messages.push(Message {
                    role: Role::User,
                    content: result_msg,
                });
                // Assistant tool-call + tool-result message just pushed both join the
                // pinned tail — the NEXT loop iteration's `refit` must not trim them.
                pinned_tail_len += 2;

                // Checkpoint after each tool call. Progress saturates at 90 until completion.
                if let Some(wi_id) = &work_item_id {
                    let progress = ((tool_index + 1) * 10).min(90) as i64;
                    let checkpoint_json = serde_json::json!({
                        "tool_index": tool_index,
                        "last_tool": &tool_name
                    })
                    .to_string();
                    let _ = work_items::checkpoint(
                        &db,
                        wi_id,
                        Some(tool_name.as_str()),
                        progress,
                        &checkpoint_json,
                    )
                    .await;
                }

                tool_index += 1;
            } else {
                // The common case: a plain-text answer with no tool call. Every safe
                // byte of `response` was already forwarded live by
                // `stream_llm_response` above — `strip_tool_markup` here is a no-op
                // pass (there's no tag to strip) kept only so the DB-persisted
                // `final_response` and the trace/skill-synthesis text stay identical
                // in shape to the pre-streaming behavior.
                final_text_already_streamed = true;
                break 'turn Ok(tool_call::strip_tool_markup(&response));
            }
        }
    };

    // Finalize the WorkItem on ALL exit paths — success, tool failure, or LLM error.
    if let Some(wi_id) = &work_item_id {
        match &loop_result {
            Err(e) => {
                let _ = work_items::fail(&db, wi_id, &format!("{e:#}")).await;
            }
            Ok(_) => {
                let any_error = tool_call_log.iter().any(|e| e["ok"] == false);
                if any_error {
                    let _ = work_items::fail(&db, wi_id, "One or more tool calls failed").await;
                } else {
                    let _ = work_items::complete(&db, wi_id).await;
                }
            }
        }
    }

    let final_response = loop_result?;

    let tokens = estimate_tokens(&final_response);
    sessions::insert_message(&db, &session_id, "assistant", &final_response, Some(tokens)).await?;

    // Record task trace for skill synthesis — 3-way outcome (phase-08, F22):
    // Failure only when the final response itself signals inability or more than
    // half the tool calls in this turn failed; Partial when some (but not most)
    // failed; Success otherwise. Replaces the old binary "failure if ANY call
    // errored," which made a 9-out-of-10-success turn indistinguishable from a
    // total failure in the EMA reward this trace eventually drives.
    //
    // Harness Completion phase 5: label provenance + telemetry, then (only when a
    // real signal fired) drive the previously-dead EMA — see
    // `record_outcome_and_update_skill`'s doc comment for the shared undo/repeat/
    // label/insert-trace/EMA sequence this also runs for `run_sub_turn`. m4's exact
    // undo predicate needs a `created_at` to compare against — using "now" internally
    // (rather than whatever `insert_trace` mints moments later) is a negligible skew
    // against a 5-minute window and avoids a second DB round-trip just to read the
    // row back before checking undo.
    let elapsed_ms = turn_start.elapsed().as_millis() as i64;
    // C2 (Phase 8) — supersedes the prior H2 review note: `turn_prompt_tokens`/
    // `turn_completion_tokens` are populated from the LAST LLM call's
    // `StreamChunk::Done` frame, gated on `stream_llm_response`'s llama-vs-cloud
    // provenance signal (see that function's doc comment and the loop body above).
    // A llama-backed turn persists real measurements; a cloud-backed turn persists
    // `None` — still no fabricated `estimate_tokens`-style guess (CLAUDE.md "real
    // code only"), just now a genuine value where one actually exists.
    record_outcome_and_update_skill(
        &db,
        &session_id,
        &req.message,
        &tool_call_log,
        &tools,
        &final_response,
        elapsed_ms,
        OutcomeMetricsInput {
            // Auto Model Routing R1 (phase 4): the tier actually used for the turn's LAST
            // LLM call — `current_tier` reflects any escalation that fired, matching the
            // same "last call's counts" convention `turn_prompt_tokens`/
            // `turn_completion_tokens` already follow above. `None` (session default,
            // routing disabled, or no heuristic triggered) persists as NULL.
            model_tier: current_tier.map(routing::tier_label),
            prompt_tokens: turn_prompt_tokens,
            completion_tokens: turn_completion_tokens,
            delegate_overhead_ms: None, // L0 turns have no delegate-spawn overhead of their own.
            confidence_update_failure_msg: "failed to update skill confidence from outcome label",
            // M3 review fix: the L0 turn is the SOLE owner of learning — see
            // `OutcomeMetricsInput::owns_learning`'s doc comment.
            owns_learning: true,
            approval_gate: &tool_ctx.approval_gate,
            final_turn_deletes: turn_deletes.load(std::sync::atomic::Ordering::Relaxed),
            turn_id: turn_id_str.as_str(),
        },
    )
    .await;

    // Auto Model Routing R1 (phase 4): best-effort routing-decision telemetry, co-located
    // with the trace write above — best-effort (`let _ =`, never `?`) and written ONLY
    // when routing is enabled, so `routing_enabled=false` produces ZERO rows (the identity
    // guarantee `turn_integration_tests`/golden harness rely on). A crashed turn writes
    // nothing (see migration 0031's write contract) — this call sits after every fallible
    // `?` in this function has already resolved, so it only ever runs on a completed turn.
    if routing_active {
        let escalated_to = if current_tier != decision.tier { current_tier } else { None };
        let new_row = routing_decisions::NewRoutingDecision {
            turn_id: turn_id_str.as_str(),
            run_id: None, // an L0 chat turn is not a pipeline run
            context_kind: "chat",
            stage_kind: None,
            chosen_tier: decision.tier.map(routing::tier_label),
            escalated_to: escalated_to.map(routing::tier_label),
            decision_source: decision.source.as_label(),
            cost_quality: i64::from(llm.cost_quality()),
            feature_msg_words: decision.features.msg_words as i64,
            feature_has_code: decision.features.has_code,
            feature_history_user_msgs: decision.features.history_user_msgs as i64,
            feature_depth: decision.features.depth_label,
            escalation_trigger: escalated_to.map(|_| "stream_init_error"),
            prior_failures: i64::from(escalated_to.is_some()),
        };
        if let Err(e) = routing_decisions::insert(&db, new_row).await {
            tracing::warn!(turn = %turn_id, "routing_decisions insert failed: {e:#}");
        }
    }

    // Only send `final_response` here if it was never streamed live during the loop
    // (the loop-guard's fallback message) — the common plain-text-answer path already
    // delivered every safe byte as `ResponseChunk::Text` increments via
    // `stream_llm_response`, and resending the full string here would duplicate it.
    if !final_text_already_streamed && !final_response.is_empty() {
        let _ = tx.send(ResponseChunk::Text(final_response)).await;
    }

    // Auto Model Routing R1 (phase 5) transparency badge — emitted ONLY when routing is
    // enabled, so a `routing_enabled=false` turn produces the exact legacy chunk stream
    // (no `TurnMeta` at all). Never emitted from `run_sub_turn`/the pipeline runner's own
    // synthetic `Complete` sites — the badge is strictly an L0-turn concept.
    if routing_active {
        let badge = build_turn_meta_badge(&llm.snapshot(), current_tier);
        let _ = tx.send(ResponseChunk::TurnMeta { badge: Some(badge) }).await;
    }
    let _ = tx.send(ResponseChunk::Complete).await;

    Ok(())
}

#[cfg(test)]
mod turn_integration_tests {
    //! Harness Completion phase 2 gap-closure: unlike `sub_turn_tests`/`outcome_tests`
    //! above (which drive `run_sub_turn` directly against a scripted cloud completion),
    //! these tests drive the FULL `run_turn` entrypoint end-to-end — the one that mints
    //! `turn_id`/`turn_deletes` and is what `Orchestrator::process` actually calls — over
    //! a REAL SSE stream (the wire format `complete_stream`/`cloud.rs` speak, not the
    //! plain-JSON shape `complete_tiered`'s mock servers above use), through REAL
    //! `tool_call::dispatch` calls against REAL tools (`TaskCreateTool`/`TaskDeleteTool`/
    //! `DelegateTool`), asserting on what actually landed in a REAL `action_journal` table.
    //!
    //! This closes two gaps a haily-tester review found in the phase-2 unit tests: (1)
    //! `turn_id` grouping was only proven with hand-constructed `ToolContext`s sharing a
    //! literal string, never `run_turn`'s own minted id; (2) the M2 cap's cross-delegation
    //! cumulative count was only proven with a hand-seeded counter, never a real
    //! `run_turn` → `delegate_to_*` → `run_sub_turn` chain.
    use super::*;
    use crate::delegate::DelegateTool;
    use haily_db::queries::journal;
    use haily_db::DbHandle;
    use haily_kms::KmsHandle;
    use haily_llm::LlmConfig;
    use haily_tools::v1::tasks::{TaskCreateTool, TaskDeleteTool};
    use haily_tools::ToolRegistry;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::RwLock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn cloud_config(base_url: String) -> LlmConfig {
        LlmConfig {
            cloud_api_keys: vec!["test-key".to_string()],
            cloud_base_url: base_url,
            cloud_model: "test-model".to_string(),
            ..LlmConfig::default()
        }
    }

    async fn test_db_kms() -> (Arc<DbHandle>, Arc<KmsHandle>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("haily.db");
        let db = Arc::new(DbHandle::init(&db_path).await.expect("db init"));
        let kms = Arc::new(
            KmsHandle::init((*db).clone(), dir.path())
                .await
                .expect("kms init"),
        );
        (db, kms, dir)
    }

    /// Real SSE (`text/event-stream`) responder speaking the SAME wire dialect
    /// `cloud.rs`'s `complete_stream` parses (unlike the plain-JSON mocks used by
    /// `outcome_tests`/`sub_turn_tests` above, which only ever exercise
    /// `complete_tiered`/`complete`). One accepted TCP connection = one LLM call;
    /// `contents[n]` is streamed as a single `data:` delta for the Nth call this
    /// server receives, then `data: [DONE]`. A call index beyond the scripted list
    /// repeats the LAST entry, so a test can under-script and still get a
    /// deterministic final answer instead of a hung/reset connection.
    async fn spawn_scripted_sse_server(contents: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let call_count = Arc::new(AtomicUsize::new(0));
        let contents = Arc::new(contents);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let call_count = Arc::clone(&call_count);
                let contents = Arc::clone(&contents);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = stream.read(&mut buf).await;

                    let n = call_count.fetch_add(1, Ordering::SeqCst);
                    let idx = n.min(contents.len().saturating_sub(1));
                    let content = contents
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| "Final answer.".to_string());

                    let delta = serde_json::json!({
                        "choices": [{ "delta": { "content": content } }]
                    })
                    .to_string();
                    let sse_body = format!("data: {delta}\n\ndata: [DONE]\n\n");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse_body}"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        format!("http://{addr}")
    }

    fn tool_call_content(tool: &str, args: serde_json::Value) -> String {
        format!(r#"<tool_call>{{"tool":"{tool}","args":{args}}}</tool_call>"#)
    }

    /// Plain-JSON (NON-streaming) scripted responder — the dialect `LlmRouter::complete`/
    /// `complete_tiered` speak (`cloud.rs`'s `complete`, not `complete_stream`). A
    /// delegated `run_sub_turn` calls `llm.complete_tiered(..)`, never `complete_stream`,
    /// so its mock server must NOT be the SSE one `run_turn`'s own L0 loop requires —
    /// using the SSE responder here would silently hand back an unparsed SSE body as a
    /// literal `"choices"`-less JSON blob and the sub-turn's tool-call loop would never
    /// see a tool call at all.
    async fn spawn_scripted_json_server(contents: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let call_count = Arc::new(AtomicUsize::new(0));
        let contents = Arc::new(contents);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let call_count = Arc::clone(&call_count);
                let contents = Arc::clone(&contents);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = stream.read(&mut buf).await;

                    let n = call_count.fetch_add(1, Ordering::SeqCst);
                    let idx = n.min(contents.len().saturating_sub(1));
                    let content = contents
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| "Final answer.".to_string());

                    let payload = serde_json::json!({
                        "choices": [{ "message": { "content": content } }]
                    })
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

    /// **Test 1 (Gap 1).** Drives a REAL `run_turn` whose mock L0 LLM makes TWO real
    /// `task_create` tool calls in the SAME turn, then queries the journal by
    /// `session_id` (not a hand-picked `turn_id`) and proves both rows share the ONE
    /// `turn_id` `run_turn` itself minted — collectible together via `list_by_turn`,
    /// exactly as `journal_undo`'s `turn_id` form (and `undo_turn`) rely on.
    #[tokio::test]
    async fn run_turn_groups_two_real_tool_calls_under_one_minted_turn_id() {
        let (db, kms, _dir) = test_db_kms().await;

        let base_url = spawn_scripted_sse_server(vec![
            tool_call_content("task_create", serde_json::json!({"title": "First"})),
            tool_call_content("task_create", serde_json::json!({"title": "Second"})),
            "Đã tạo xong hai việc.".to_string(),
        ])
        .await;
        let llm = Arc::new(LlmRouter::init(cloud_config(base_url)).await);

        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(TaskCreateTool));

        let runtime = TurnRuntime {
            db: db.clone(),
            kms,
            llm,
            tools: Arc::new(registry),
            kill: Arc::new(AtomicBool::new(false)),
            routing_enabled: Arc::new(AtomicBool::new(true)),
            view_store: Arc::new(crate::view::ViewStore::new()),
        };
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(64);
        // Drain response chunks concurrently so `run_turn`'s streaming sends never
        // block on a full/unread channel.
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let req = Request {
            session_id: uuid::Uuid::new_v4(),
            adapter_id: "test-adapter".to_string(),
            message: "please create two tasks".to_string(),
            user_ref: None,
            depth: Default::default(),
            origin: Default::default(),
        };

        run_turn(&req, runtime, tx, &broker, &cancel)
            .await
            .expect("run_turn");
        drain.await.expect("drain task");

        let rows = journal::list_by_session(&db, &req.session_id.to_string())
            .await
            .expect("list_by_session");
        assert_eq!(
            rows.len(),
            2,
            "both task_create calls of this turn must be journaled"
        );

        let turn_ids: std::collections::HashSet<&str> = rows
            .iter()
            .map(|r| r.turn_id.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(
            turn_ids.len(),
            1,
            "both rows must share the SAME turn_id run_turn minted, got: {turn_ids:?}"
        );
        let turn_id = *turn_ids.iter().next().unwrap();
        assert!(
            !turn_id.is_empty(),
            "turn_id must actually be stamped (not left null)"
        );
        assert!(
            uuid::Uuid::parse_str(turn_id).is_ok(),
            "turn_id must be the real minted UUID, not a placeholder: {turn_id}"
        );

        // Collectible together via list_by_turn — the exact query journal_undo's
        // `turn_id` form (and undo_turn) rely on for group-undo.
        let via_turn = journal::list_by_turn(&db, turn_id, &req.session_id.to_string())
            .await
            .expect("list_by_turn");
        assert_eq!(via_turn.len(), 2, "list_by_turn must collect both rows");
    }

    /// **Test 2 (Gap 2).** Drives a REAL `run_turn` where the L0 mock LLM issues
    /// `MAX_AUTO_DELETES_PER_TURN - 1` (4) real re-tiered `task_delete` calls directly,
    /// then calls a REAL `delegate_to_helper` tool, whose sub-turn (a SEPARATE mock
    /// LLM, proving the two levels are genuinely distinct completions) issues 2 more
    /// real `task_delete` calls. The M2 per-turn cap (`MAX_AUTO_DELETES_PER_TURN = 5`)
    /// must trigger on the 6th delete OVERALL — the 2nd one inside the sub-turn — proving
    /// `ctx.turn_deletes` is the SAME shared counter across the delegation boundary, not
    /// reset when `run_sub_turn` starts. The escalated call is auto-denied here (the
    /// approval-gate mechanics are already covered by `tool_call.rs`'s unit tests); what
    /// this test proves is that the escalation fires AT ALL, and fires at the cumulative
    /// 6th call rather than a fresh per-sub-turn 2nd call.
    #[tokio::test]
    async fn cross_delegation_delete_cap_is_cumulative_not_reset_per_subturn() {
        let (db, kms, _dir) = test_db_kms().await;

        // Pre-seed 6 real tasks so each scripted task_delete call has a real row to
        // find (a delete against a nonexistent id is a silent no-op that never reaches
        // the journal or increments turn_deletes — see local_journaled_write's
        // pre-check). ids[0..4) are deleted at L0, ids[4..6) inside the sub-turn.
        let mut ids = Vec::with_capacity(6);
        for i in 0..6 {
            let t = haily_db::queries::tasks::insert(
                &db,
                &format!("cap-task-{i}"),
                None,
                "medium",
                None,
                None,
            )
            .await
            .expect("seed task");
            ids.push(t.id);
        }

        // L0 script: 4 deletes, then delegate, then a final answer once the delegate
        // tool result comes back.
        let l0_url = spawn_scripted_sse_server(vec![
            tool_call_content("task_delete", serde_json::json!({"id": ids[0]})),
            tool_call_content("task_delete", serde_json::json!({"id": ids[1]})),
            tool_call_content("task_delete", serde_json::json!({"id": ids[2]})),
            tool_call_content("task_delete", serde_json::json!({"id": ids[3]})),
            tool_call_content(
                "delegate_to_helper",
                serde_json::json!({"task": "cleanup more"}),
            ),
            "Đã dọn dẹp xong.".to_string(),
        ])
        .await;
        let l0_llm = Arc::new(LlmRouter::init(cloud_config(l0_url)).await);

        // Sub-turn script: a DIFFERENT mock server/completion stream — proves the two
        // levels are genuinely distinct LLM calls, not the same response reused. Uses
        // the plain-JSON dialect (`spawn_scripted_json_server`), not SSE: `run_sub_turn`
        // calls `llm.complete_tiered` (→ `complete`), never `complete_stream`.
        let sub_url = spawn_scripted_json_server(vec![
            tool_call_content("task_delete", serde_json::json!({"id": ids[4]})),
            tool_call_content("task_delete", serde_json::json!({"id": ids[5]})),
            "Đã dọn xong phần còn lại.".to_string(),
        ])
        .await;
        let sub_llm = Arc::new(LlmRouter::init(cloud_config(sub_url)).await);

        let mut sub_registry = ToolRegistry::new();
        sub_registry.register(Arc::new(TaskDeleteTool));

        let kill = Arc::new(AtomicBool::new(false));
        let delegate = DelegateTool {
            tool_name: "delegate_to_helper",
            description: "delegates cleanup work to a helper sub-agent",
            system_prompt: "You are a helper sub-agent.",
            domain_name: "helper",
            db: db.clone(),
            kms: kms.clone(),
            llm: Arc::new(RwLock::new(sub_llm)),
            sub_registry: Arc::new(sub_registry),
            max_depth: 2,
            model_tier: None,
            kill: Arc::clone(&kill),
        };

        let mut l0_registry = ToolRegistry::new();
        l0_registry.register(Arc::new(TaskDeleteTool));
        l0_registry.register(Arc::new(delegate));

        let runtime = TurnRuntime {
            db: db.clone(),
            kms,
            llm: l0_llm,
            tools: Arc::new(l0_registry),
            kill,
            routing_enabled: Arc::new(AtomicBool::new(true)),
            view_store: Arc::new(crate::view::ViewStore::new()),
        };
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(64);

        let session_id = uuid::Uuid::new_v4();
        // Drain + auto-deny the escalated approval this test expects to be raised —
        // mirrors tool_call.rs's own cap-escalation tests' responder pattern.
        let broker_for_responder = Arc::clone(&broker);
        let approval_seen = Arc::new(AtomicUsize::new(0));
        let approval_seen_writer = Arc::clone(&approval_seen);
        let responder = tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                if let ResponseChunk::ToolApprovalRequest { approval_id, .. } = chunk {
                    approval_seen_writer.fetch_add(1, Ordering::SeqCst);
                    use haily_types::ApprovalResolver;
                    broker_for_responder.resolve(approval_id, session_id, false);
                }
            }
        });

        let req = Request {
            session_id,
            adapter_id: "test-adapter".to_string(),
            message: "delete these six tasks, delegate the rest".to_string(),
            user_ref: None,
            depth: Default::default(),
            origin: Default::default(),
        };

        run_turn(&req, runtime, tx, &broker, &cancel)
            .await
            .expect("run_turn");
        responder.await.expect("responder task");

        assert_eq!(
            approval_seen.load(Ordering::SeqCst),
            1,
            "exactly one delete (the 6th overall, 2nd inside the sub-turn) must have \
             escalated to the approval gate — proving turn_deletes is cumulative across \
             the delegation boundary rather than reset per sub-turn"
        );

        // Corroborate via the journal: only 5 deletes actually executed (the escalated
        // 6th was denied, so local_journaled_write's transaction never ran for it) and
        // every executed delete shares the ONE turn_id, spanning both L0 and the
        // delegated sub-turn.
        let rows = journal::list_by_session(&db, &session_id.to_string())
            .await
            .expect("list_by_session");
        let delete_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.tool_name == "task_delete")
            .collect();
        assert_eq!(
            delete_rows.len(),
            5,
            "only the 5 auto-run deletes (under the cap) reach the journal; the denied \
             6th never executes: {delete_rows:?}"
        );
        let turn_ids: std::collections::HashSet<&str> = delete_rows
            .iter()
            .map(|r| r.turn_id.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(
            turn_ids.len(),
            1,
            "L0 deletes and the sub-turn's deletes must share the SAME turn_id: {turn_ids:?}"
        );

        // The 5th task (last one under the cap) was actually deleted...
        let remaining_active = haily_db::queries::tasks::active(&db).await.expect("active");
        assert!(
            !remaining_active.iter().any(|t| t.id == ids[4]),
            "the 5th (under-cap) delete must have actually executed"
        );
        // ...but the 6th's delete was DENIED, so its task must still be active — the
        // cap-escalation must have actually blocked execution, not just raised a prompt
        // nobody's answer affected.
        assert!(
            remaining_active.iter().any(|t| t.id == ids[5]),
            "the 6th (denied, over-cap) task must survive undeleted"
        );
    }

    /// Auto Model Routing R1 (phase 5): `routing_enabled=true` on a plain message (no
    /// heuristic trigger, `tier=None`) must still emit exactly one `TurnMeta` chunk,
    /// arriving immediately before `Complete`, whose badge is the session-default model
    /// name (no `tier ·` prefix — there is no tier to show).
    #[tokio::test]
    async fn routing_enabled_emits_turn_meta_badge_before_complete() {
        let (db, kms, _dir) = test_db_kms().await;
        let base_url = spawn_scripted_sse_server(vec!["Hello there.".to_string()]).await;
        let llm = Arc::new(LlmRouter::init(cloud_config(base_url)).await);

        let runtime = TurnRuntime {
            db: db.clone(),
            kms,
            llm,
            tools: Arc::new(ToolRegistry::new()),
            kill: Arc::new(AtomicBool::new(false)),
            routing_enabled: Arc::new(AtomicBool::new(true)),
            view_store: Arc::new(crate::view::ViewStore::new()),
        };
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(64);

        let req = Request {
            session_id: uuid::Uuid::new_v4(),
            adapter_id: "test-adapter".to_string(),
            message: "hi".to_string(),
            user_ref: None,
            depth: Default::default(),
            origin: Default::default(),
        };

        run_turn(&req, runtime, tx, &broker, &cancel)
            .await
            .expect("run_turn");

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }

        let last_two: Vec<&ResponseChunk> = chunks.iter().rev().take(2).collect();
        match (last_two.first(), last_two.get(1)) {
            (Some(ResponseChunk::Complete), Some(ResponseChunk::TurnMeta { badge })) => {
                assert_eq!(
                    badge.as_deref(),
                    Some("test-model"),
                    "a tier=None turn's badge must be the bare session-default model name"
                );
            }
            other => panic!(
                "expected [.., TurnMeta, Complete] as the last two chunks, got: {other:?}"
            ),
        }
        assert_eq!(
            chunks
                .iter()
                .filter(|c| matches!(c, ResponseChunk::TurnMeta { .. }))
                .count(),
            1,
            "exactly one TurnMeta chunk must be emitted per turn, got: {chunks:?}"
        );
    }

    /// CRITICAL: `routing_enabled=false` must reproduce the exact legacy chunk stream —
    /// no `TurnMeta` chunk at all, only the pre-phase-5 `Text`/`Complete` sequence.
    #[tokio::test]
    async fn routing_disabled_emits_no_turn_meta_chunk() {
        let (db, kms, _dir) = test_db_kms().await;
        let base_url = spawn_scripted_sse_server(vec!["Hello there.".to_string()]).await;
        let llm = Arc::new(LlmRouter::init(cloud_config(base_url)).await);

        let runtime = TurnRuntime {
            db: db.clone(),
            kms,
            llm,
            tools: Arc::new(ToolRegistry::new()),
            kill: Arc::new(AtomicBool::new(false)),
            routing_enabled: Arc::new(AtomicBool::new(false)),
            view_store: Arc::new(crate::view::ViewStore::new()),
        };
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(64);

        let req = Request {
            session_id: uuid::Uuid::new_v4(),
            adapter_id: "test-adapter".to_string(),
            message: "hi".to_string(),
            user_ref: None,
            depth: Default::default(),
            origin: Default::default(),
        };

        run_turn(&req, runtime, tx, &broker, &cancel)
            .await
            .expect("run_turn");

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }
        assert!(
            !chunks.iter().any(|c| matches!(c, ResponseChunk::TurnMeta { .. })),
            "routing_enabled=false must emit ZERO TurnMeta chunks, got: {chunks:?}"
        );
        assert!(matches!(chunks.last(), Some(ResponseChunk::Complete)));
    }
}

#[cfg(test)]
mod outcome_signal_tests {
    //! Harness Completion phase 5 — end-to-end through the REAL `run_turn` entrypoint
    //! (mirrors `turn_integration_tests`'s SSE mock-server technique): the outcome
    //! signal that used to be dead code now drives `update_skill_confidence`, gated
    //! by the anti-reinforcement `unknown`-never-moves-confidence invariant, and
    //! `req.message` is the ONLY text `detect_feedback` ever sees (the m2 attribution
    //! boundary this suite proves by construction of the call graph, not by
    //! inspecting internals).
    use super::*;
    use haily_db::queries::skills as db_skills;
    use haily_db::DbHandle;
    use haily_kms::skills::TaskOutcome;
    use haily_kms::KmsHandle;
    use haily_llm::LlmConfig;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn cloud_config(base_url: String) -> LlmConfig {
        LlmConfig {
            cloud_api_keys: vec!["test-key".to_string()],
            cloud_base_url: base_url,
            cloud_model: "test-model".to_string(),
            ..LlmConfig::default()
        }
    }

    async fn test_db_kms() -> (Arc<DbHandle>, Arc<KmsHandle>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("haily.db");
        let db = Arc::new(DbHandle::init(&db_path).await.expect("db init"));
        let kms = Arc::new(
            KmsHandle::init((*db).clone(), dir.path())
                .await
                .expect("kms init"),
        );
        (db, kms, dir)
    }

    /// Single-shot SSE responder — every call gets the SAME plain-text final answer,
    /// no tool calls. Sufficient for tests that only care about the outcome/label
    /// wiring after the loop ends, not about tool dispatch.
    async fn spawn_plain_answer_sse_server(answer: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = stream.read(&mut buf).await;
                    let delta = serde_json::json!({
                        "choices": [{ "delta": { "content": answer } }]
                    })
                    .to_string();
                    let sse_body = format!("data: {delta}\n\ndata: [DONE]\n\n");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse_body}"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        format!("http://{addr}")
    }

    pub(super) async fn run_plain_turn(
        db: Arc<DbHandle>,
        kms: Arc<KmsHandle>,
        session_id: uuid::Uuid,
        message: &str,
        answer: &'static str,
    ) {
        run_plain_turn_with_routing(db, kms, session_id, message, answer, true).await;
    }

    /// Auto Model Routing R1 phase 4: same drive as `run_plain_turn`, but with the
    /// `routing_enabled` seed under caller control — used by the identity/blast-radius
    /// tests that must prove behavior with the kill switch OFF vs its default ON state.
    pub(super) async fn run_plain_turn_with_routing(
        db: Arc<DbHandle>,
        kms: Arc<KmsHandle>,
        session_id: uuid::Uuid,
        message: &str,
        answer: &'static str,
        routing_enabled: bool,
    ) {
        let base_url = spawn_plain_answer_sse_server(answer).await;
        let llm = Arc::new(LlmRouter::init(cloud_config(base_url)).await);
        let runtime = TurnRuntime {
            db: db.clone(),
            kms,
            llm,
            tools: Arc::new(ToolRegistry::new()),
            kill: Arc::new(AtomicBool::new(false)),
            routing_enabled: Arc::new(AtomicBool::new(routing_enabled)),
            view_store: Arc::new(crate::view::ViewStore::new()),
        };
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let req = Request {
            session_id,
            adapter_id: "test-adapter".to_string(),
            message: message.to_string(),
            user_ref: None,
            depth: Default::default(),
            origin: Default::default(),
        };
        run_turn(&req, runtime, tx, &broker, &cancel)
            .await
            .expect("run_turn");
        drain.await.expect("drain task");
    }

    pub(super) async fn latest_trace(db: &DbHandle, session_id: uuid::Uuid) -> db_skills::TaskTrace {
        db_skills::most_recent_trace(db, &session_id.to_string())
            .await
            .expect("most_recent_trace")
            .expect("a trace must have been recorded")
    }

    /// Scripted SSE responder that emits `contents[n]` (repeating the last entry past
    /// the end) as this call's delta — mirrors `turn_integration_tests`'
    /// `spawn_scripted_sse_server`, duplicated here (not shared) per this file's own
    /// per-module-helper convention (see that module's docs).
    async fn spawn_scripted_sse_server(contents: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let contents = std::sync::Arc::new(contents);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let call_count = std::sync::Arc::clone(&call_count);
                let contents = std::sync::Arc::clone(&contents);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = stream.read(&mut buf).await;
                    let n = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let idx = n.min(contents.len().saturating_sub(1));
                    let content = contents.get(idx).cloned().unwrap_or_else(|| "Final answer.".to_string());
                    let delta = serde_json::json!({
                        "choices": [{ "delta": { "content": content } }]
                    })
                    .to_string();
                    let sse_body = format!("data: {delta}\n\ndata: [DONE]\n\n");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse_body}"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        format!("http://{addr}")
    }

    fn tool_call_content(tool: &str, args: serde_json::Value) -> String {
        format!(r#"<tool_call>{{"tool":"{tool}","args":{args}}}</tool_call>"#)
    }

    /// Plain-JSON (non-streaming) scripted responder — the dialect `complete_tiered`
    /// speaks, needed for a delegated sub-turn's completions (M3 test). See
    /// `turn_integration_tests::spawn_scripted_json_server`'s doc comment for why
    /// this must NOT be the SSE responder.
    async fn spawn_scripted_json_server(contents: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let contents = std::sync::Arc::new(contents);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let call_count = std::sync::Arc::clone(&call_count);
                let contents = std::sync::Arc::clone(&contents);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = stream.read(&mut buf).await;
                    let n = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let idx = n.min(contents.len().saturating_sub(1));
                    let content = contents.get(idx).cloned().unwrap_or_else(|| "Final answer.".to_string());
                    let payload = serde_json::json!({
                        "choices": [{ "message": { "content": content } }]
                    })
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

    /// Drives one turn against a SCRIPTED (multi-response) SSE server with the REAL
    /// `feedback_react` tool registered — used for the M2 corroboration test, where
    /// turn 2 must issue an explicit `feedback_react` call in the SAME turn as the
    /// repeat-request text, unlike `run_plain_turn`'s empty registry.
    async fn run_scripted_turn_with_feedback_tool(
        db: Arc<DbHandle>,
        kms: Arc<KmsHandle>,
        session_id: uuid::Uuid,
        message: &str,
        scripted_responses: Vec<String>,
    ) {
        let base_url = spawn_scripted_sse_server(scripted_responses).await;
        let llm = Arc::new(LlmRouter::init(cloud_config(base_url)).await);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(haily_tools::v1::memory::FeedbackReactTool));
        let runtime = TurnRuntime {
            db: db.clone(),
            kms,
            llm,
            tools: Arc::new(registry),
            kill: Arc::new(AtomicBool::new(false)),
            routing_enabled: Arc::new(AtomicBool::new(true)),
            view_store: Arc::new(crate::view::ViewStore::new()),
        };
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let req = Request {
            session_id,
            adapter_id: "test-adapter".to_string(),
            message: message.to_string(),
            user_ref: None,
            depth: Default::default(),
            origin: Default::default(),
        };
        run_turn(&req, runtime, tx, &broker, &cancel)
            .await
            .expect("run_turn");
        drain.await.expect("drain task");
    }

    /// A plain-text, no-tool-call turn (no undo, no repeat-request, Success outcome)
    /// has no corroborating signal at all — the label must be `unknown`, and the turn's
    /// trace must carry NO label_source/label_confidence.
    #[tokio::test]
    async fn a_plain_successful_turn_with_no_signal_is_labeled_unknown() {
        let (db, kms, _dir) = test_db_kms().await;
        let session_id = uuid::Uuid::new_v4();

        run_plain_turn(db.clone(), kms, session_id, "what's the weather like", "It's sunny today.")
            .await;

        let trace = latest_trace(&db, session_id).await;
        assert_eq!(trace.outcome, "success");
        assert!(
            trace.label_source.is_none(),
            "a no-signal turn must not be force-labeled: {:?}",
            trace.label_source
        );
        assert!(trace.label_confidence.is_none());
    }

    /// SAFETY (anti-reinforcement invariant): running an `unknown`-labeled turn must
    /// NOT move a matching skill's confidence at all — even when an active skill
    /// exists whose description closely matches the turn's task. This is the
    /// end-to-end proof that `run_turn` actually SKIPS `update_skill_confidence` for
    /// `unknown` rather than defaulting to a neutral reward.
    #[tokio::test]
    async fn unknown_labeled_turn_does_not_move_a_matching_skills_confidence() {
        let (db, kms, _dir) = test_db_kms().await;
        let session_id = uuid::Uuid::new_v4();

        let skill = db_skills::insert_skill(
            &db,
            "weather-lookup",
            "check the weather forecast for a city",
            "pattern",
            "[]",
        )
        .await
        .expect("seed skill");
        let confidence_before = skill.confidence;

        run_plain_turn(
            db.clone(),
            kms,
            session_id,
            "check the weather forecast for hanoi today",
            "It's sunny today.",
        )
        .await;

        let after = db_skills::get_skill(&db, &skill.id)
            .await
            .expect("get_skill")
            .expect("skill must still exist");
        assert_eq!(
            after.confidence, confidence_before,
            "an unknown-labeled turn must leave skill confidence UNCHANGED, not nudge it toward a neutral value"
        );
    }

    /// A genuine repeat-request (same session, near-duplicate task text on the very
    /// next turn) CORROBORATED by an explicit negative `feedback_react` call within
    /// that same turn (M2 review fix — an uncorroborated repeat alone must NOT label
    /// as a failure signal; see `uncorroborated_repeat_request_leaves_confidence_unchanged`
    /// below for that negative case) drives a `RepeatRequest` label, which — since it
    /// is NOT unknown — DOES move a matching skill's confidence via the EMA. This
    /// proves the "success turn moves confidence" success criterion: the previously-
    /// dead `update_skill_confidence` path is now reachable end-to-end.
    #[tokio::test]
    async fn corroborated_repeat_request_label_moves_a_matching_skills_confidence_via_ema() {
        let (db, kms, _dir) = test_db_kms().await;
        let session_id = uuid::Uuid::new_v4();

        // Description and both turns' messages deliberately reuse the SAME core word
        // set so every pairwise Jaccard comparison this test relies on (skill-match
        // AND turn-to-turn repeat-request) clears `CLUSTER_SIMILARITY_THRESHOLD`
        // (0.40) with margin, rather than depending on a borderline overlap ratio.
        let skill = db_skills::insert_skill(
            &db,
            "flight-booking",
            "book a flight ticket to hanoi for the user",
            "pattern",
            "[]",
        )
        .await
        .expect("seed skill");
        // Seed confidence BELOW the eventual EMA reward (outcome.ema_reward()=1.0 *
        // label.confidence=REPEAT_REQUEST_CONFIDENCE=0.5 → reward=0.5) so the
        // production EMA_ALPHA=0.10 update provably moves it upward: seeding AT or
        // ABOVE 0.5 would make `alpha*0.5 + (1-alpha)*confidence` a no-op-or-decrease,
        // hiding a real wiring bug behind a coincidental equality.
        db_skills::update_skill_confidence(&db, &skill.id, 0.2, 1.0)
            .await
            .expect("seed low confidence");
        let mid = db_skills::get_skill(&db, &skill.id)
            .await
            .unwrap()
            .unwrap()
            .confidence;
        assert!((mid - 0.2).abs() < 1e-9, "sanity: confidence seeded at 0.2");

        // Turn 1: establishes the prior trace's task_description.
        run_plain_turn(
            db.clone(),
            kms.clone(),
            session_id,
            "book a flight ticket to hanoi",
            "Sure, let me help with that.",
        )
        .await;
        // Turn 2: near-duplicate of turn 1's task (triggers is_repeat_request) AND an
        // explicit negative feedback_react call in the SAME turn (the M2
        // corroborating signal) — the model issues the tool call first, then a final
        // answer once the tool result comes back.
        run_scripted_turn_with_feedback_tool(
            db.clone(),
            kms,
            session_id,
            "book a flight ticket to hanoi please",
            vec![
                tool_call_content(
                    "feedback_react",
                    serde_json::json!({"reaction": "negative", "about": "accuracy"}),
                ),
                "Xin lỗi, để mình thử lại.".to_string(),
            ],
        )
        .await;

        let trace = latest_trace(&db, session_id).await;
        assert_eq!(trace.label_source.as_deref(), Some("repeat_request"));

        let after = db_skills::get_skill(&db, &skill.id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            after.confidence > mid,
            "a Success outcome with a non-unknown label must move confidence upward via EMA, got {} (was {mid})",
            after.confidence
        );
    }

    /// M2 review fix, negative case: a repeat-request with NO corroborating negative
    /// signal (a clean, all-succeeded turn, no explicit feedback) must NOT move a
    /// matching skill's confidence at all — the same anti-reinforcement invariant as
    /// `unknown_labeled_turn_does_not_move_a_matching_skills_confidence`, but reached
    /// via an uncorroborated `repeat_request` rather than a first-time no-signal turn.
    /// This is the direct end-to-end proof that benign habitual repetition (e.g. a
    /// daily "tóm tắt hôm nay" habit) no longer erodes skill confidence.
    #[tokio::test]
    async fn uncorroborated_repeat_request_leaves_confidence_unchanged() {
        let (db, kms, _dir) = test_db_kms().await;
        let session_id = uuid::Uuid::new_v4();

        let skill = db_skills::insert_skill(
            &db,
            "flight-booking",
            "book a flight ticket to hanoi for the user",
            "pattern",
            "[]",
        )
        .await
        .expect("seed skill");
        let confidence_before = skill.confidence;

        // Turn 1: establishes the prior trace's task_description.
        run_plain_turn(
            db.clone(),
            kms.clone(),
            session_id,
            "book a flight ticket to hanoi",
            "Sure, let me help with that.",
        )
        .await;
        // Turn 2: near-duplicate of turn 1 (would trigger is_repeat_request) but NO
        // tool call, NO feedback signal, NO failure — a completely clean, benign
        // repeat with zero corroborating negative indicator.
        run_plain_turn(
            db.clone(),
            kms,
            session_id,
            "book a flight ticket to hanoi please",
            "Sure, let me help with that.",
        )
        .await;

        let trace = latest_trace(&db, session_id).await;
        assert!(
            trace.label_source.is_none(),
            "an uncorroborated repeat must be labeled unknown (NULL), not repeat_request: {:?}",
            trace.label_source
        );

        let after = db_skills::get_skill(&db, &skill.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.confidence, confidence_before,
            "a benign, uncorroborated repeat must leave skill confidence UNCHANGED"
        );
    }

    /// m2 attribution: `req.message` — a genuine incoming user message — carrying a
    /// negative-feedback phrase downgrades the PRIOR turn's trace to failure.
    #[tokio::test]
    async fn genuine_user_message_negative_feedback_downgrades_the_prior_trace() {
        let (db, kms, _dir) = test_db_kms().await;
        let session_id = uuid::Uuid::new_v4();

        // Turn 1: an ordinary request, Success outcome.
        run_plain_turn(
            db.clone(),
            kms.clone(),
            session_id,
            "what's the capital of vietnam",
            "Hanoi is the capital of Vietnam.",
        )
        .await;
        let turn1_trace_id = latest_trace(&db, session_id).await.id;

        // Turn 2: the user's own typed message says the previous answer was wrong.
        // `detect_feedback` runs on `req.message` — this call site — and NOTHING else.
        run_plain_turn(
            db.clone(),
            kms,
            session_id,
            "sai rồi, không phải vậy",
            "Xin lỗi, để mình kiểm tra lại.",
        )
        .await;

        let turn1_after = db_skills::recent_traces(&db, 10)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == turn1_trace_id)
            .expect("turn 1's trace must still exist");
        assert_eq!(
            turn1_after.outcome, "failure",
            "a genuine user negative-feedback message must downgrade the PRIOR turn's trace"
        );
        assert_eq!(turn1_after.label_source.as_deref(), Some("phrase_feedback"));
    }

    /// m2 SECURITY boundary: a negative-feedback phrase embedded in a TOOL RESULT
    /// (simulated here directly, since `run_turn` has no tool that echoes attacker
    /// text back as `req.message`) must NOT be able to downgrade a trace — because
    /// `detect_feedback` is only ever invoked on `req.message`, never on tool output.
    /// This test proves the negative: feeding the same phrase through the ONLY OTHER
    /// text channel a turn produces (the assistant's own final response, which is
    /// never re-parsed as feedback either) has no downgrade effect, corroborating
    /// that the call graph — not a runtime filter — is what makes tool/pasted content
    /// unreachable as a feedback source.
    #[tokio::test]
    async fn a_negative_phrase_in_the_assistant_response_never_downgrades_anything() {
        let (db, kms, _dir) = test_db_kms().await;
        let session_id = uuid::Uuid::new_v4();

        // The ASSISTANT's response (not the user's message) contains a negative
        // phrase — this text flows through `sessions::insert_message`/streaming, but
        // is NEVER passed to `detect_feedback` (only `req.message` is, at the top of
        // `run_turn`, before this response is even generated).
        run_plain_turn(
            db.clone(),
            kms,
            session_id,
            "how do I center a div",
            "sai rồi, không phải vậy — let me try again with flexbox instead.",
        )
        .await;

        let trace = latest_trace(&db, session_id).await;
        assert_eq!(
            trace.outcome, "success",
            "a negative phrase appearing in the ASSISTANT's own output (never in \
             req.message) must not downgrade the turn's own trace"
        );
    }

    /// Metrics persistence: a turn's trace carries `tool_call_count`. C2 (Phase 8,
    /// supersedes the prior H2 review note): `run_plain_turn` drives `run_turn`
    /// through the CLOUD backend (`spawn_plain_answer_sse_server`) — no dialect this
    /// crate speaks exposes a real prompt-token usage field, so `StreamChunk::Done`'s
    /// `prompt_tokens` is `None` for this call, and `run_turn` must persist BOTH
    /// `prompt_tokens`/`completion_tokens` as honest `NULL`, never a fabricated
    /// frame-count number (see `stream_llm_response`'s doc comment for the full
    /// llama-vs-cloud provenance contract; the llama-side `Some` case is proven by
    /// `streaming_tests::done_frame_prompt_tokens_some_is_threaded_through_unmodified`,
    /// since a real llama.cpp model isn't available in this test environment).
    #[tokio::test]
    async fn turn_trace_persists_tool_call_count_and_leaves_unmeasured_token_fields_null() {
        let (db, kms, _dir) = test_db_kms().await;
        let session_id = uuid::Uuid::new_v4();

        run_plain_turn(db.clone(), kms, session_id, "hi", "hello there").await;

        let trace = latest_trace(&db, session_id).await;
        assert_eq!(trace.tool_call_count, Some(0));
        assert!(
            trace.completion_tokens.is_none(),
            "completion_tokens must be honest NULL, not a fabricated frame-count number, got {:?}",
            trace.completion_tokens
        );
        assert!(
            trace.prompt_tokens.is_none(),
            "prompt_tokens must be honest NULL, not an estimate_tokens heuristic, got {:?}",
            trace.prompt_tokens
        );
    }

    /// M3 review fix: a delegated turn's sub-turn STILL inserts its own trace
    /// (telemetry stands) but must NOT itself drive the EMA — only the parent L0
    /// turn's own end-of-turn `record_outcome_and_update_skill` call owns learning.
    /// Without the `owns_learning` gate, this scenario would move the matching
    /// skill's confidence TWICE for one user-visible delegated action (undocumented
    /// 2x learning rate) — proven here by giving BOTH levels a genuine, independently
    /// non-unknown label (L0: `undo_within_5min`; sub-turn: an explicit negative
    /// `feedback_react` call) so a double-EMA bug would be unambiguously visible as
    /// "moved further than one application of the EMA formula could produce."
    #[tokio::test]
    async fn delegated_turn_trace_exists_but_skill_confidence_moves_exactly_once() {
        use crate::delegate::DelegateTool;
        use haily_db::queries::{journal, sessions};
        use std::sync::RwLock;

        let (db, kms, _dir) = test_db_kms().await;
        let session_id = uuid::Uuid::new_v4();

        // Both the L0 message and the sub-turn's task text share the SAME core word
        // set as the skill's description, so `find_matching_skill` matches at BOTH
        // call sites — the precondition for a double-count bug to be observable.
        let skill = db_skills::insert_skill(
            &db,
            "trip-planning",
            "plan a trip to hanoi for the user",
            "pattern",
            "[]",
        )
        .await
        .expect("seed skill");
        db_skills::update_skill_confidence(&db, &skill.id, 0.2, 1.0)
            .await
            .expect("seed low confidence");
        let before = db_skills::get_skill(&db, &skill.id).await.unwrap().unwrap().confidence;
        assert!((before - 0.2).abs() < 1e-9, "sanity: confidence seeded at 0.2");

        // L0's own real signal: a same-session action_journal row already undone
        // within the 5-minute window — independent of anything the sub-turn does,
        // so L0's OWN label is guaranteed UndoWithinN (never unknown).
        sessions::create_session(&db, &session_id.to_string(), "test-adapter", None)
            .await
            .expect("seed session");
        let row = journal::insert(
            &db,
            journal::NewAction {
                session_id: &session_id.to_string(),
                tool_name: "odoo_create",
                tool_tier: "IrreversibleWrite",
                compensability: "compensatable",
                idempotency_key: "m3-test-op-1",
                correlation_ref: "corr-m3-1",
                request_params: r#"{"model":"res.partner"}"#,
                pre_state: None,
                pre_state_version: None,
                compensation_plan: Some(r#"{"op":"unlink","id":1}"#),
                turn_id: None,
                retention_days: 30,
                manifest_hash: None,
            },
        )
        .await
        .expect("seed journal row");
        journal::advance_undo_status(&db, &row.id, "undone")
            .await
            .expect("mark undone");

        // Sub-turn script: an explicit negative feedback_react call (guaranteed
        // non-unknown label regardless of repeat/undo signals at THIS level), then a
        // final answer.
        let sub_url = spawn_scripted_json_server(vec![
            tool_call_content(
                "feedback_react",
                serde_json::json!({"reaction": "negative", "about": "accuracy"}),
            ),
            "Đã ghi nhận, mình sẽ điều chỉnh.".to_string(),
        ])
        .await;
        let sub_llm = Arc::new(LlmRouter::init(cloud_config(sub_url)).await);

        let mut sub_registry = ToolRegistry::new();
        sub_registry.register(Arc::new(haily_tools::v1::memory::FeedbackReactTool));

        let kill = Arc::new(AtomicBool::new(false));
        let delegate = DelegateTool {
            tool_name: "delegate_to_helper",
            description: "delegates trip planning to a helper sub-agent",
            system_prompt: "You are a helper sub-agent.",
            domain_name: "helper",
            db: db.clone(),
            kms: kms.clone(),
            llm: Arc::new(RwLock::new(sub_llm)),
            sub_registry: Arc::new(sub_registry),
            max_depth: 2,
            model_tier: None,
            kill: Arc::clone(&kill),
        };

        let mut l0_registry = ToolRegistry::new();
        l0_registry.register(Arc::new(delegate));

        // L0 script: delegate once, then a final answer once the delegate's result
        // comes back.
        let l0_url = spawn_scripted_sse_server(vec![
            tool_call_content(
                "delegate_to_helper",
                serde_json::json!({"task": "plan a trip to hanoi for the user"}),
            ),
            "Đã lên kế hoạch chuyến đi Hà Nội cho bạn.".to_string(),
        ])
        .await;
        let l0_llm = Arc::new(LlmRouter::init(cloud_config(l0_url)).await);

        let runtime = TurnRuntime {
            db: db.clone(),
            kms,
            llm: l0_llm,
            tools: Arc::new(l0_registry),
            kill,
            routing_enabled: Arc::new(AtomicBool::new(true)),
            view_store: Arc::new(crate::view::ViewStore::new()),
        };
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let req = Request {
            session_id,
            adapter_id: "test-adapter".to_string(),
            message: "plan a trip to hanoi for the user".to_string(),
            user_ref: None,
            depth: Default::default(),
            origin: Default::default(),
        };
        run_turn(&req, runtime, tx, &broker, &cancel)
            .await
            .expect("run_turn");
        drain.await.expect("drain task");

        // Both traces must exist: the sub-turn's own (telemetry value stands) AND
        // the L0 turn's own.
        let all_traces = db_skills::recent_traces(&db, 10).await.expect("recent_traces");
        let session_traces: Vec<_> = all_traces
            .into_iter()
            .filter(|t| t.session_id == session_id.to_string())
            .collect();
        assert_eq!(
            session_traces.len(),
            2,
            "both the sub-turn's own trace AND the L0 turn's trace must be inserted, got: {session_traces:?}"
        );
        assert!(
            session_traces.iter().any(|t| t.label_source.as_deref() == Some("undo_within_n_min")),
            "the L0 turn's own trace must carry the undo_within_5min label"
        );
        assert!(
            session_traces.iter().any(|t| t.task_description.contains("[helper]")),
            "the sub-turn's own trace must exist with its [domain] task_description prefix"
        );

        // The skill's confidence must have moved EXACTLY ONE EMA application's worth
        // — not two. Compute the expected single-application value from the L0
        // turn's own label (UndoWithinN, confidence=UNDO_LABEL_CONFIDENCE) and the
        // Success outcome's ema_reward()=1.0, and assert the actual result matches
        // that arithmetic (not a further-moved, double-applied value).
        let expected_single_reward =
            TaskOutcome::Success.ema_reward() * haily_kms::skills::UNDO_LABEL_CONFIDENCE;
        let expected_after_one = 0.10 * expected_single_reward + 0.90 * before;

        let after = db_skills::get_skill(&db, &skill.id).await.unwrap().unwrap();
        assert!(
            (after.confidence - expected_after_one).abs() < 1e-6,
            "skill confidence must move by EXACTLY one EMA application \
             (expected {expected_after_one}, got {}) — a double-count bug would move \
             it further via a second application on top of the first",
            after.confidence
        );
    }
}

#[cfg(test)]
mod routing_toggle_tests {
    //! Auto Model Routing R1 phase 4 — the two blast-radius criteria the plan's Success
    //! Criteria table marks CRITICAL: `routing_enabled=false` must reproduce legacy
    //! behavior byte-for-byte AT THIS HARNESS'S GRANULARITY (visible text, tool
    //! dispatch, DB rows, TaskOutcome — the collector tracks no chunk sequence, so this
    //! does not overclaim byte-identical streaming; see the phase file's Assumptions),
    //! and zero `routing_decisions` rows; `routing_enabled=true` with a plain message
    //! that trips no heuristic must select the SAME tier (`None`) and add exactly one
    //! telemetry row.
    use super::outcome_signal_tests::{run_plain_turn, run_plain_turn_with_routing};
    use super::*;
    use haily_llm::LlmConfig;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn test_db_kms() -> (Arc<DbHandle>, Arc<KmsHandle>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("haily.db");
        let db = Arc::new(DbHandle::init(&db_path).await.expect("db init"));
        let kms = Arc::new(
            KmsHandle::init((*db).clone(), dir.path())
                .await
                .expect("kms init"),
        );
        (db, kms, dir)
    }

    /// CRITICAL: `routing_enabled=false` ⇒ identical outcome to a plain turn (text/DB/
    /// TaskOutcome parity at this harness's granularity) AND zero `routing_decisions`
    /// rows — the routing layer must be a complete no-op when the kill switch is off.
    #[tokio::test]
    async fn routing_disabled_matches_legacy_behavior_and_writes_zero_decision_rows() {
        let (db, kms, _dir) = test_db_kms().await;
        let session_id = uuid::Uuid::new_v4();

        run_plain_turn_with_routing(
            db.clone(),
            kms,
            session_id,
            "what's the capital of vietnam",
            "Hanoi is the capital of Vietnam.",
            false,
        )
        .await;

        // Text/DB/TaskOutcome parity: the same assistant/user message pair and outcome a
        // routing-enabled plain turn produces (see `a_plain_successful_turn_with_no_signal_is_labeled_unknown`).
        let trace = super::outcome_signal_tests::latest_trace(&db, session_id).await;
        assert_eq!(trace.outcome, "success");
        assert!(trace.label_source.is_none());

        let messages = haily_db::queries::sessions::recent_messages(&db, &session_id.to_string(), 10)
            .await
            .expect("recent_messages");
        assert!(
            messages.iter().any(|m| m.role == "assistant" && m.content == "Hanoi is the capital of Vietnam."),
            "assistant response must persist exactly as scripted, got: {messages:?}"
        );

        let rows = routing_decisions::list_recent(&db, 10).await.expect("list_recent");
        assert!(
            rows.is_empty(),
            "routing_enabled=false must write ZERO routing_decisions rows, got: {rows:?}"
        );
    }

    /// CRITICAL: `routing_enabled=true` (the shipping default) with a plain message that
    /// trips no heuristic (short, no code fence, no history) must select `tier=None` —
    /// the SAME model selection as a pre-phase-4 turn — with a blast radius of exactly
    /// one `routing_decisions` row (telemetry only; no behavior change).
    #[tokio::test]
    async fn routing_enabled_default_on_with_no_heuristic_trigger_has_blast_radius_of_one_row() {
        let (db, kms, _dir) = test_db_kms().await;
        let session_id = uuid::Uuid::new_v4();

        run_plain_turn(db.clone(), kms, session_id, "hi there", "Hello! How can I help?").await;

        let rows = routing_decisions::list_recent(&db, 10).await.expect("list_recent");
        assert_eq!(
            rows.len(),
            1,
            "blast radius must be exactly one telemetry row, got: {rows:?}"
        );
        assert_eq!(
            rows[0].chosen_tier, None,
            "a short plain message with no history/heuristic trigger must select tier=None \
             (identical model selection to a pre-phase-4 turn), got: {:?}",
            rows[0].chosen_tier
        );
        assert_eq!(rows[0].decision_source, "default");
        assert!(rows[0].escalated_to.is_none());
    }

    /// A server that fails EVERY connection pre-first-token — mirrors `crate::routing`'s
    /// own `spawn_server(usize::MAX, ..)` failure branch, duplicated here (not shared)
    /// per this file's own per-module-helper convention.
    async fn spawn_always_failing_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = stream.read(&mut buf).await;
                    // A malformed status line forces an immediate synchronous parse
                    // error — a deterministic, fast pre-first-token failure.
                    let _ = stream.write_all(b"not a valid http response\r\n\r\n").await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// Closes the gap Stage 3 (adversarial) review found untested: with the kill switch
    /// OFF, a stream failure must reproduce the legacy hard-fail EVEN when a tier
    /// override is configured — the escalation rescue must not fire, because
    /// `escalation_policy.enabled` is gated on `routing_active`, not unconditional.
    /// Before this gate, the no-op guard alone caught only the no-override case,
    /// leaving this exact (OFF + override + failure) intersection able to silently
    /// switch models on a turn the user believes has routing fully disabled.
    #[tokio::test]
    async fn routing_disabled_never_escalates_even_with_a_tier_override_configured() {
        let (db, kms, _dir) = test_db_kms().await;
        let session_id = uuid::Uuid::new_v4();

        let base_url = spawn_always_failing_server().await;
        let mut config = LlmConfig {
            cloud_api_keys: vec!["test-key".to_string()],
            cloud_base_url: base_url,
            cloud_model: "test-model".to_string(),
            ..LlmConfig::default()
        };
        // A distinct override — if escalation fired despite the kill switch, the no-op
        // guard alone would not stop it (this is the exact config the gap allowed).
        config.tier_models.medium = Some("distinct-medium-model".to_string());
        let llm = Arc::new(LlmRouter::init(config).await);

        let runtime = TurnRuntime {
            db: db.clone(),
            kms,
            llm,
            tools: Arc::new(ToolRegistry::new()),
            kill: Arc::new(AtomicBool::new(false)),
            routing_enabled: Arc::new(AtomicBool::new(false)),
            view_store: Arc::new(crate::view::ViewStore::new()),
        };
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let req = Request {
            session_id,
            adapter_id: "test-adapter".to_string(),
            message: "hi".to_string(),
            user_ref: None,
            depth: Default::default(),
            origin: Default::default(),
        };

        let result = run_turn(&req, runtime, tx, &broker, &cancel).await;
        drain.await.expect("drain task");

        assert!(
            result.is_err(),
            "routing_enabled=false must reproduce the legacy hard-fail — no escalation \
             rescue may fire even with a tier override configured, got: {result:?}"
        );

        let rows = routing_decisions::list_recent(&db, 10).await.expect("list_recent");
        assert!(
            rows.is_empty(),
            "a failed turn with routing off must still write zero routing_decisions rows, got: {rows:?}"
        );
    }
}
