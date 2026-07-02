/// Main agent turn: user message → LLM → tool loop → final response.
use anyhow::Result;
use haily_db::{
    queries::{sessions, skills as db_skills, work_items},
    DbHandle,
};
use haily_types::{Request, ResponseChunk};
use haily_kms::KmsHandle;
use haily_llm::{CompletionRequest, LlmClient, LlmRouter, Message, Role, StreamChunk};
use haily_tools::{ToolContext, ToolRegistry};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

use crate::{approval::ApprovalBroker, budget, context, feedback_parser, tag_matcher, tool_call};

fn estimate_tokens(s: &str) -> i64 {
    (s.len() / 4) as i64
}

/// Splits `buffer` into `(emit, hold)` at the hold-back boundary: `hold` is the
/// longest trailing suffix that could still extend into a recognized tool tag
/// (`<tool_call>`/`<tool_result>`, whitespace/case tolerant — see `tag_matcher`) if
/// more text arrives; `emit` is everything before it, safe to show the user now.
///
/// This only answers "could the tail still become an OPENING tag" — it does not by
/// itself know about an already-confirmed, still-open tag body (that's
/// `stream_llm_response`'s `in_tag` state, tracked separately). Pure function so the
/// exhaustive boundary cases (tag split across chunks, mid-chunk, variant tags, no
/// tag) are unit-testable without a channel.
fn split_safe(buffer: &str) -> (&str, &str) {
    let hold_len = tag_matcher::holdback_len(buffer);
    buffer.split_at(buffer.len() - hold_len)
}

/// Consumes a `complete_stream` channel, forwarding safe text increments to `tx` as
/// `ResponseChunk::Text` while withholding any tool-tag body in full — from the
/// moment an opening tag is confirmed until its matching closing tag is found — and
/// returns the FULL accumulated raw response text — identical in shape to what
/// `complete()` would have returned, so callers (`run_turn`'s tool-call loop) can
/// keep parsing/dispatching against the complete string exactly as before.
///
/// Two-state machine over the growing buffer:
/// - `Scanning`: no confirmed open tag yet. `split_safe` withholds only the trailing
///   prefix-of-a-tag; once a FULL open tag is confirmed inside the withheld portion,
///   switch to `InTag` and withhold everything from that tag's `<` onward.
/// - `InTag`: withhold everything (never call `split_safe`, never emit) until the
///   matching close tag is found in the buffer, then resume `Scanning` from just
///   past the close tag.
///
/// SECURITY: text is only ever forwarded to `tx` while `Scanning` and past
/// `split_safe`'s hold-back boundary — this is the boundary that stops partial (or
/// even complete but unapproved) tool-call JSON from ever reaching the user before
/// `tool_call::dispatch`'s approval gate runs (see phase-06 spec's Security
/// Considerations).
///
/// Cancellation: selects on `cancel.cancelled()` alongside `rx.recv()` so a fired
/// token ends consumption within one channel-poll, without waiting for the
/// producer (llama's blocking decode loop / the cloud SSE task) to notice on its own
/// — dropping `rx` here is itself the second half of the cancellation signal those
/// producers watch for (see `llama.rs`/`cloud.rs` doc comments).
async fn stream_llm_response(
    rx: &mut mpsc::Receiver<StreamChunk>,
    tx: &mpsc::Sender<ResponseChunk>,
    cancel: &CancellationToken,
) -> Result<String> {
    let mut full = String::new();
    // Buffer of bytes not yet flushed to `tx`. While `Scanning`, holds only the
    // tail that might still become a tag; while `InTag`, holds the entire withheld
    // tag body seen so far (never flushed until the close tag resolves it).
    let mut pending = String::new();
    let mut in_tag = false;

    loop {
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(anyhow::anyhow!("turn cancelled"));
            }
            chunk = rx.recv() => chunk,
        };

        match chunk {
            Some(StreamChunk::Token(piece)) => {
                full.push_str(&piece);
                pending.push_str(&piece);

                // Drain as many complete open/close tag transitions as the buffer
                // currently supports — a single Token piece could conceivably close
                // out a tag AND open another in pathological model output.
                loop {
                    if in_tag {
                        match tag_matcher::find_next_tag(&pending, 0) {
                            Some(m) if m.closing => {
                                // Matching close found: the whole tag body (open
                                // through close) stays withheld from `tx` forever —
                                // only text AFTER it re-enters the safe-to-emit path.
                                pending = pending[m.end..].to_string();
                                in_tag = false;
                                continue; // re-scan: more content may already be buffered
                            }
                            _ => break, // still inside the tag body — wait for more input
                        }
                    } else {
                        // A fully-formed OPEN tag anywhere in `pending` must be
                        // checked directly — `split_safe`/`holdback_len` only reasons
                        // about an as-yet-*unresolved* trailing prefix, so a tag that
                        // is already fully closed within `pending` (e.g. an entire
                        // `<tool_call>...</tool_call>` arriving in one Token piece)
                        // would otherwise sail through as "nothing pending" and leak.
                        match tag_matcher::find_next_tag(&pending, 0) {
                            Some(m) if !m.closing => {
                                let before = &pending[..m.start];
                                if !before.is_empty() {
                                    let text = before.to_string();
                                    let _ = tx.send(ResponseChunk::Text(text)).await;
                                }
                                pending = pending[m.start..].to_string();
                                in_tag = true;
                                continue; // re-scan in InTag state immediately
                            }
                            Some(m) => {
                                // A stray CLOSING tag before any open tag — routine when a
                                // weak model echoes the `</tool_result>` framing injected
                                // into context each round. Emit the safe text before it,
                                // DROP the stray token (never shown to the user), and keep
                                // scanning the remainder: a genuine `<tool_call>` block can
                                // follow in the same buffer and must not be handed to the
                                // suffix-only `split_safe`, which would leak it verbatim.
                                let before = &pending[..m.start];
                                if !before.is_empty() {
                                    let text = before.to_string();
                                    let _ = tx.send(ResponseChunk::Text(text)).await;
                                }
                                pending = pending[m.end..].to_string();
                                continue;
                            }
                            None => {
                                // No confirmed tag yet — fall back to the trailing-prefix
                                // hold-back for the still-ambiguous tail (e.g. a lone '<'
                                // or a partial tag name).
                                let (emit, hold) = split_safe(&pending);
                                if !emit.is_empty() {
                                    let text = emit.to_string();
                                    let _ = tx.send(ResponseChunk::Text(text)).await;
                                    pending = hold.to_string();
                                }
                                break;
                            }
                        }
                    }
                }
            }
            Some(StreamChunk::Done { .. }) => {
                // Any residual `pending` text at clean end-of-stream was never
                // confirmed to close out a tag (a real closed tag would already have
                // been drained above) — either an incomplete tag prefix (e.g. a lone
                // trailing '<') or, rarer, an unterminated `<tool_call>` the model
                // never closed. Either way it's already included in `full` for the
                // caller's `parse_tool_call`/`strip_tool_markup` pass, so it must NOT
                // be flushed here too — an unterminated tag left in `pending` must
                // stay invisible to the user (the security invariant this function
                // exists for), and a plain incomplete prefix will be re-rendered by
                // `strip_tool_markup` at the loop's end instead.
                return Ok(full);
            }
            Some(StreamChunk::Error(msg)) => {
                return Err(anyhow::anyhow!("{msg}"));
            }
            None => {
                // Channel closed without a Done/Error — treat as an abnormal end
                // rather than silently returning a truncated success.
                return Err(anyhow::anyhow!("LLM stream ended without a completion signal"));
            }
        }
    }
}

/// Parameters for a stateless sub-agent turn.
///
/// Groups the per-call request (`task`, `system_prompt`, `domain_name`, `depth`)
/// with the shared runtime handles so `run_sub_turn` stays within a sane arity.
pub struct SubTurnRequest {
    pub task: String,
    pub system_prompt: &'static str,
    pub domain_name: &'static str,
    /// Nesting depth this sub-turn runs at (1 = L1, 2 = L2). Propagated to `ToolContext`.
    pub depth: u8,
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    pub llm: Arc<LlmRouter>,
    /// Domain-filtered tool registry the sub-agent is allowed to use.
    pub tools: Arc<ToolRegistry>,
    /// Parent session id — shared so KMS search and skill traces stay unified.
    pub session_id: uuid::Uuid,
}

/// Stateless sub-agent turn for domain/specialist agents.
///
/// Differences from `run_turn`:
/// - No session history loaded — receives only `task` message.
/// - No WorkItem tracking — parent turn's WorkItem covers the whole task.
/// - No session message persistence — sub-agent output is returned inline.
/// - KMS search still runs with the parent session_id for relevant facts.
/// - `depth` is propagated to `ToolContext` so delegate tools can enforce max_depth.
#[instrument(skip_all, fields(depth = req.depth, domain = %req.domain_name))]
pub async fn run_sub_turn(req: SubTurnRequest) -> Result<String> {
    let SubTurnRequest {
        task,
        system_prompt,
        domain_name,
        depth,
        db,
        kms,
        llm,
        tools,
        session_id,
    } = req;
    let turn_start = std::time::Instant::now();

    // Hybrid KMS search for relevant facts, same as the parent turn.
    let mut ctx = kms.build_life_context(session_id).await?;
    let search_results = kms.search_hybrid(&task, 8).await.unwrap_or_default();
    ctx.relevant_facts = search_results.into_iter().map(|r| r.text).collect();

    let tool_block = context::tool_reference_block(&tools);
    let full_prompt = format!(
        "{system_prompt}\n\n## Tool Calling\nKhi cần dùng tool, output ĐÚNG format này:\n<tool_call>{{\"tool\":\"name\",\"args\":{{...}}}}</tool_call>\n\nSau khi nhận tool result, tiếp tục trả lời bình thường.\n\n## Available Tools\n{tool_block}"
    );

    let messages = vec![
        haily_llm::Message::system(full_prompt),
        haily_llm::Message::user(task.clone()),
    ];

    let tool_ctx = ToolContext {
        db: db.clone(),
        kms,
        session_id,
        depth,
    };

    // Reuse the same tool loop logic as run_turn, without WorkItem tracking.
    let mut msgs = messages;
    let mut guard = tool_call::LoopGuard::new();
    let mut tool_call_log: Vec<serde_json::Value> = Vec::new();
    let (tx, _rx) = tokio::sync::mpsc::channel(32); // sink — sub-agents don't stream to user

    // Sub-turns run at depth > 0, which `dispatch` hard-blocks from
    // `ToolClass::RequireApproval` before either of these is ever touched — a
    // throwaway broker/token (not threaded from the parent turn) is intentional,
    // not a shortcut: a sub-agent must never reach the approval path at all.
    let sub_turn_broker = ApprovalBroker::new();
    let sub_turn_cancel = CancellationToken::new();

    // No DB history to load for a stateless sub-turn (`msgs` starts as just
    // `[system, task]`), but the tool loop still accumulates `<tool_result>`
    // messages that can grow unbounded — same overflow risk as `run_turn`, so the
    // same re-fit-per-iteration budgeting applies. `pinned_tail_len` starts at 1
    // (the task message) and grows by 2 per tool round-trip.
    let token_budget = budget::TokenBudget::new(llm.context_window());
    let mut pinned_tail_len: usize = 1;

    let final_response = loop {
        msgs = token_budget.refit(&msgs, pinned_tail_len);

        let llm_req = CompletionRequest::simple(msgs.clone());
        let response = llm.complete(llm_req).await?;

        if let Some((tool_name, args)) = tool_call::parse_tool_call(&response) {
            msgs.push(haily_llm::Message { role: haily_llm::Role::Assistant, content: response.clone() });

            // Guard BEFORE dispatch: a tripped guard (duplicate call / ceiling) ends
            // the sub-turn instead of feeding an error back — which a looping local
            // model would otherwise spin on indefinitely.
            if let Err(e) = guard.check(&tool_name, &args) {
                tracing::warn!(error = %e, "sub-turn loop guard tripped — ending");
                break tool_call::strip_tool_markup(&response);
            }

            let (result, tool_ok) = tool_call::dispatch(
                &tool_name,
                args.clone(),
                &tools,
                &tool_ctx,
                &tx,
                &sub_turn_broker,
                &sub_turn_cancel,
            )
            .await
            .unwrap_or_else(|e| (format!("Error: {e:#}"), false));

            tool_call_log.push(serde_json::json!({
                "tool": &tool_name,
                "args": args.to_string(),
                "ok": tool_ok,
            }));

            // Neutralize tool-protocol tags in the (possibly untrusted) result
            // before feeding it back — defuses second-order prompt injection.
            let safe_result = tool_call::strip_tool_tags(&result);
            let result_msg = format!(
                "<tool_result>{{\"tool\":\"{tool_name}\",\"result\":{},\"ok\":{}}}</tool_result>",
                serde_json::Value::String(safe_result),
                tool_ok
            );
            msgs.push(haily_llm::Message { role: haily_llm::Role::User, content: result_msg });
            // Assistant tool-call + tool-result just pushed both join the pinned
            // tail — the NEXT iteration's `refit` must not trim them.
            pinned_tail_len += 2;
        } else {
            break tool_call::strip_tool_markup(&response);
        }
    };

    // Record sub-agent activity for skill synthesis — uses the parent session_id
    // so the skill system learns from delegated work too.
    let elapsed_ms = turn_start.elapsed().as_millis() as i64;
    let tool_calls_json = serde_json::to_string(&tool_call_log).unwrap_or_default();
    let outcome = if tool_call_log.iter().any(|e| e["ok"] == false) { "failure" } else { "success" };
    let sub_task = format!("[{domain_name}] {task}");
    let _ = db_skills::insert_trace(
        &db,
        &session_id.to_string(),
        &sub_task,
        &tool_calls_json,
        outcome,
        Some(elapsed_ms),
    )
    .await;

    Ok(final_response)
}

/// Shared runtime handles for a full turn — grouped so `run_turn` stays within a
/// sane arity (mirrors `SubTurnRequest`'s reasoning for sub-turns). These are the
/// same handles `Orchestrator` already holds as fields; `process` just forwards them.
pub struct TurnRuntime {
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    pub llm: Arc<LlmRouter>,
    pub tools: Arc<ToolRegistry>,
}

/// Full agent turn. Called once per incoming Request.
///
/// `broker` gates `ToolClass::RequireApproval` tool calls at depth 0; `cancel` is the
/// turn's cancellation token — firing it (shutdown) denies any pending approval
/// immediately instead of holding up the drain for up to the 120s approval timeout.
#[instrument(skip_all, fields(session = %req.session_id))]
pub async fn run_turn(
    req: &Request,
    runtime: TurnRuntime,
    tx: mpsc::Sender<ResponseChunk>,
    broker: &ApprovalBroker,
    cancel: &CancellationToken,
) -> Result<()> {
    let TurnRuntime { db, kms, llm, tools } = runtime;
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

    // Detect and persist feedback signal before inserting user message
    if let Some(signal) = feedback_parser::detect_feedback(&req.message) {
        let _ = feedback_parser::apply_feedback_signal(&signal, &db).await;
    }

    sessions::insert_message(&db, &session_id, "user", &req.message, None).await?;
    info!(session = session_id, "processing user message");

    let context_window = llm.context_window();
    let token_budget = budget::TokenBudget::new(context_window);
    let (mut messages, _ctx) =
        context::build_messages(&kms, &db, &tools, &session_id, &req.message, context_window)
            .await?;

    let tool_ctx = ToolContext {
        db: db.clone(),
        kms: kms.clone(),
        session_id: req.session_id,
        depth: 0,
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
            let mut stream = match llm.complete_stream(llm_req).await {
                Ok(rx) => rx,
                Err(e) => break 'turn Err(e),
            };
            let response = match stream_llm_response(&mut stream, &tx, cancel).await {
                Ok(r) => r,
                Err(e) => break 'turn Err(e),
            };

            if let Some((tool_name, args)) = tool_call::parse_tool_call(&response) {
                messages.push(Message { role: Role::Assistant, content: response.clone() });

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
                    tool_call::dispatch(&tool_name, args.clone(), &tools, &tool_ctx, &tx, broker, cancel)
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
                messages.push(Message { role: Role::User, content: result_msg });
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

    // Record task trace for skill synthesis
    let elapsed_ms = turn_start.elapsed().as_millis() as i64;
    let tool_calls_json = serde_json::to_string(&tool_call_log).unwrap_or_default();
    let outcome = if tool_call_log.iter().any(|e| e["ok"] == false) { "failure" } else { "success" };
    let _ = db_skills::insert_trace(
        &db,
        &session_id,
        &req.message,
        &tool_calls_json,
        outcome,
        Some(elapsed_ms),
    )
    .await;

    // Only send `final_response` here if it was never streamed live during the loop
    // (the loop-guard's fallback message) — the common plain-text-answer path already
    // delivered every safe byte as `ResponseChunk::Text` increments via
    // `stream_llm_response`, and resending the full string here would duplicate it.
    if !final_text_already_streamed && !final_response.is_empty() {
        let _ = tx.send(ResponseChunk::Text(final_response)).await;
    }
    let _ = tx.send(ResponseChunk::Complete).await;

    Ok(())
}

#[cfg(test)]
mod streaming_tests {
    //! Phase 6 — hold-back streaming. `split_safe` is exhaustively unit-tested here
    //! (pure function, no async needed); `stream_llm_response` is tested against a
    //! real `mpsc` channel fed canned `StreamChunk`s to prove the end-to-end
    //! consumer never lets tag bytes reach `tx` and still returns the full text for
    //! `parse_tool_call`.
    use super::*;

    #[test]
    fn split_safe_emits_everything_when_no_tag_present() {
        let (emit, hold) = split_safe("hello, how can I help?");
        assert_eq!(emit, "hello, how can I help?");
        assert_eq!(hold, "");
    }

    #[test]
    fn split_safe_withholds_tag_split_mid_word() {
        let (emit, hold) = split_safe("here you go <tool_c");
        assert_eq!(emit, "here you go ");
        assert_eq!(hold, "<tool_c");
    }

    #[test]
    fn split_safe_withholds_full_tag_awaiting_close_bracket() {
        let (emit, hold) = split_safe("ok <tool_call");
        assert_eq!(emit, "ok ");
        assert_eq!(hold, "<tool_call");
    }

    #[test]
    fn split_safe_emits_full_tag_once_confirmed_complete() {
        // A CLOSED tag is not held back by split_safe itself — the caller
        // (`stream_llm_response`) still accumulates it into `full` for
        // `parse_tool_call`, but split_safe's own contract is purely "could this
        // still extend into a tag", which a terminated `>` answers no to.
        let (emit, hold) = split_safe("<tool_call>{}</tool_call>");
        assert_eq!(emit, "<tool_call>{}</tool_call>");
        assert_eq!(hold, "");
    }

    #[test]
    fn split_safe_handles_variant_tags_case_and_whitespace() {
        let (emit, hold) = split_safe("answer <Tool_Call ");
        assert_eq!(emit, "answer ");
        assert_eq!(hold, "<Tool_Call ");
    }

    #[test]
    fn split_safe_recovers_once_bracket_content_diverges_from_any_tag() {
        // "<b>" cannot extend into tool_call/tool_result — safe to emit in full.
        let (emit, hold) = split_safe("some <b>html</b> text");
        assert_eq!(emit, "some <b>html</b> text");
        assert_eq!(hold, "");
    }

    /// Feeds `pieces` through `stream_llm_response` as `StreamChunk::Token`s
    /// followed by `Done`, and returns `(visible_text_sent_to_tx, full_return_value)`.
    async fn run_stream(pieces: &[&str]) -> (String, String) {
        let (llm_tx, mut llm_rx) = mpsc::channel(64);
        for p in pieces {
            llm_tx.send(StreamChunk::Token(p.to_string())).await.unwrap();
        }
        llm_tx.send(StreamChunk::Done { total_tokens: pieces.len() as u32 }).await.unwrap();
        drop(llm_tx);

        let (user_tx, mut user_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let full = stream_llm_response(&mut llm_rx, &user_tx, &cancel).await.unwrap();
        drop(user_tx);

        let mut visible = String::new();
        while let Some(chunk) = user_rx.recv().await {
            if let ResponseChunk::Text(t) = chunk {
                visible.push_str(&t);
            }
        }
        (visible, full)
    }

    #[tokio::test]
    async fn tool_call_split_across_three_chunks_never_leaks_to_user() {
        // "<tool_call>{"tool":"x","args":{}}</tool_call>" split across 3 arbitrary
        // chunk boundaries, including mid-tag-name.
        let (visible, full) = run_stream(&[
            "Để mình kiểm tra nhé. <tool_",
            "call>{\"tool\":\"x\",\"args\":{}}</tool_c",
            "all>",
        ])
        .await;

        assert_eq!(visible, "Để mình kiểm tra nhé. ", "zero tag bytes must reach the user");
        assert!(!visible.contains('<'), "no angle bracket of any kind may leak");
        let (tool, _args) = tool_call::parse_tool_call(&full).expect("full text must still parse");
        assert_eq!(tool, "x");
    }

    #[tokio::test]
    async fn tag_mid_chunk_is_withheld_from_first_safe_boundary() {
        let (visible, full) = run_stream(&["prefix <tool_call>{\"tool\":\"y\"}</tool_call> ignored-suffix"]).await;
        // Only the text strictly before the tag is visible; everything from '<'
        // onward in this single chunk is held back until the loop-level buffer
        // resolves it, but stream_llm_response's job is only to never leak tag
        // bytes — the trailing "ignored-suffix" after a still-open call is legitimately
        // buffered until Done, at which point `full` (not `visible`) carries it.
        assert_eq!(visible, "prefix ");
        assert!(!visible.contains("tool_call"));
        let (tool, _) = tool_call::parse_tool_call(&full).expect("must parse");
        assert_eq!(tool, "y");
    }

    #[tokio::test]
    async fn variant_tag_with_trailing_space_is_withheld_and_parses() {
        let (visible, full) = run_stream(&["ok <tool_call >{\"tool\":\"z\"}</ tool_call>"]).await;
        assert_eq!(visible, "ok ");
        assert!(!visible.to_ascii_lowercase().contains("tool_call"));
        let (tool, _) = tool_call::parse_tool_call(&full).expect("variant tags must still parse");
        assert_eq!(tool, "z");
    }

    #[tokio::test]
    async fn mixed_case_variant_tag_is_withheld_and_parses() {
        let (visible, full) = run_stream(&["<Tool_Call>{\"tool\":\"w\"}</Tool_Call>"]).await;
        assert_eq!(visible, "");
        let (tool, _) = tool_call::parse_tool_call(&full).expect("mixed-case tags must still parse");
        assert_eq!(tool, "w");
    }

    #[tokio::test]
    async fn stray_closing_tag_before_a_real_call_never_leaks_the_block() {
        // The Phase-6 review's CRITICAL case: a stray `</tool_result>` (routinely echoed
        // from injected framing) appears before a genuine `<tool_call>` in the SAME
        // chunk. The scanner must skip the stray close and withhold the whole call —
        // never hand it to the suffix-only hold-back, which would stream the JSON args.
        let (visible, full) = run_stream(&[
            r#"kết quả </tool_result> rồi <tool_call>{"tool":"x","args":{"path":"/home/secret"}}</tool_call>"#,
        ])
        .await;
        assert!(!visible.contains("tool_call"), "tool-call tag/JSON must not leak: {visible:?}");
        assert!(!visible.contains("/home/secret"), "tool args must not leak: {visible:?}");
        // The real call is still recoverable from `full` for dispatch.
        let (tool, _) = tool_call::parse_tool_call(&full).expect("real call must still parse from full");
        assert_eq!(tool, "x");
    }

    #[tokio::test]
    async fn plain_text_with_no_tag_streams_immediately_and_completely() {
        let (visible, full) = run_stream(&["Xin ", "chào, ", "hôm nay trời đẹp."]).await;
        assert_eq!(visible, "Xin chào, hôm nay trời đẹp.");
        assert_eq!(full, "Xin chào, hôm nay trời đẹp.");
    }

    #[tokio::test]
    async fn stream_error_after_partial_text_returns_err_with_partial_visible() {
        let (llm_tx, mut llm_rx) = mpsc::channel(64);
        llm_tx.send(StreamChunk::Token("partial answer".to_string())).await.unwrap();
        llm_tx.send(StreamChunk::Error("backend disconnected".to_string())).await.unwrap();
        drop(llm_tx);

        let (user_tx, mut user_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let result = stream_llm_response(&mut llm_rx, &user_tx, &cancel).await;
        drop(user_tx);

        assert!(result.is_err(), "a stream error must surface as Err, not a truncated Ok");

        let mut visible = String::new();
        while let Some(ResponseChunk::Text(t)) = user_rx.recv().await {
            visible.push_str(&t);
        }
        assert_eq!(visible, "partial answer", "text streamed before the error must still have been delivered");
    }

    #[tokio::test]
    async fn cancellation_stops_consumption_promptly() {
        let (_llm_tx, mut llm_rx) = mpsc::channel::<StreamChunk>(64); // never sends — only cancel ends this
        let (user_tx, _user_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream_llm_response(&mut llm_rx, &user_tx, &cancel),
        )
        .await
        .expect("cancellation must end consumption promptly, not hang");

        assert!(result.is_err(), "cancellation must surface as an Err so the turn fails cleanly");
    }
}
