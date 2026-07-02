/// Main agent turn: user message → LLM → tool loop → final response.
use anyhow::Result;
use haily_db::{
    queries::{sessions, skills as db_skills, work_items},
    DbHandle,
};
use haily_io::{Request, ResponseChunk};
use haily_kms::KmsHandle;
use haily_llm::{CompletionRequest, LlmClient, LlmRouter, Message, Role};
use haily_tools::{ToolContext, ToolRegistry};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, instrument};

use crate::{context, feedback_parser, tool_call};

fn estimate_tokens(s: &str) -> i64 {
    (s.len() / 4) as i64
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

    let final_response = loop {
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

            let (result, tool_ok) = tool_call::dispatch(&tool_name, args.clone(), &tools, &tool_ctx, &tx)
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

/// Full agent turn. Called once per incoming Request.
#[instrument(skip_all, fields(session = %req.session_id))]
pub async fn run_turn(
    req: &Request,
    db: Arc<DbHandle>,
    kms: Arc<KmsHandle>,
    llm: Arc<LlmRouter>,
    tools: Arc<ToolRegistry>,
    tx: mpsc::Sender<ResponseChunk>,
) -> Result<()> {
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

    let (mut messages, _ctx) =
        context::build_messages(&kms, &db, &tools, &session_id, &req.message).await?;

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

    // Capture the loop result without propagating `?` immediately so the
    // WorkItem finalization block below always runs — even when LLM calls fail
    // mid-turn after the WorkItem has already been created.
    let loop_result: Result<String> = 'turn: {
        loop {
            let llm_req = CompletionRequest::simple(messages.clone());
            let response = match llm.complete(llm_req).await {
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
                    tool_call::dispatch(&tool_name, args.clone(), &tools, &tool_ctx, &tx)
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

    if !final_response.is_empty() {
        let _ = tx.send(ResponseChunk::Text(final_response)).await;
    }
    let _ = tx.send(ResponseChunk::Complete).await;

    Ok(())
}
