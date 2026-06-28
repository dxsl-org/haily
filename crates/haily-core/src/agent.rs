/// Main agent turn: user message → LLM → tool loop → final response.
use anyhow::Result;
use haily_db::{
    queries::{sessions, skills as db_skills},
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

    let final_response = loop {
        let llm_req = CompletionRequest::simple(messages.clone());
        let response = llm.complete(llm_req).await?;

        if let Some((tool_name, args)) = tool_call::parse_tool_call(&response) {
            messages.push(Message { role: Role::Assistant, content: response.clone() });

            let result =
                tool_call::dispatch(&tool_name, args.clone(), &tools, &tool_ctx, &tx, &mut guard)
                    .await
                    .unwrap_or_else(|e| format!("Error: {e:#}"));

            tool_call_log.push(serde_json::json!({
                "tool": &tool_name,
                "args": args.to_string(),
                "ok":   !result.starts_with("Error:")
            }));

            let result_msg = format!(
                "<tool_result>{{\"tool\":\"{tool_name}\",\"result\":{},\"ok\":true}}</tool_result>",
                serde_json::Value::String(result)
            );
            messages.push(Message { role: Role::User, content: result_msg });
        } else {
            break tool_call::strip_tool_markup(&response);
        }
    };

    let tokens = estimate_tokens(&final_response);
    sessions::insert_message(&db, &session_id, "assistant", &final_response, Some(tokens)).await?;

    // Record task trace for skill synthesis
    let elapsed_ms = turn_start.elapsed().as_millis() as i64;
    let tool_calls_json = serde_json::to_string(&tool_call_log).unwrap_or_default();
    let _ = db_skills::insert_trace(
        &db,
        &session_id,
        &req.message,
        &tool_calls_json,
        "success",
        Some(elapsed_ms),
    )
    .await;

    if !final_response.is_empty() {
        let _ = tx.send(ResponseChunk::Text(final_response)).await;
    }
    let _ = tx.send(ResponseChunk::Complete).await;

    Ok(())
}
