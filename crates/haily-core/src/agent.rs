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

    // Ensure session exists in DB
    if sessions::get_session(&db, &session_id).await?.is_none() {
        sessions::create_session(&db, &req.adapter_id, req.user_ref.as_deref()).await?;
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

                // Lazy WorkItem creation: only on the first tool call of this turn.
                if work_item_id.is_none() {
                    if let Ok(wi) = work_items::create(&db, &session_id, &req.message).await {
                        let _ = work_items::start(&db, &wi.id).await;
                        work_item_id = Some(wi.id);
                    }
                }

                let result =
                    tool_call::dispatch(&tool_name, args.clone(), &tools, &tool_ctx, &tx, &mut guard)
                        .await
                        .unwrap_or_else(|e| format!("Error: {e:#}"));

                let tool_ok = !result.starts_with("Error:");
                tool_call_log.push(serde_json::json!({
                    "tool": &tool_name,
                    "args": args.to_string(),
                    "ok":   tool_ok
                }));

                let result_msg = format!(
                    "<tool_result>{{\"tool\":\"{tool_name}\",\"result\":{},\"ok\":{}}}</tool_result>",
                    serde_json::Value::String(result),
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
