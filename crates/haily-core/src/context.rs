/// Builds the LLM-ready message list and system prompt for each turn.
use haily_db::queries::sessions;
use haily_kms::{KmsHandle, LifeContext};
use haily_llm::Message;
use haily_tools::ToolRegistry;

/// Compact tool reference injected into the system prompt.
/// Each line: `- name: description` — enough for the model to know what's available.
pub fn tool_reference_block(registry: &ToolRegistry) -> String {
    let mut tools = registry.list();
    tools.sort_by_key(|t| t.name());
    tools
        .iter()
        .map(|t| format!("- **{}**: {}", t.name(), t.description()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Full system prompt = soul block + tool calling instructions + memory bullets.
pub fn build_full_system_prompt(ctx: &LifeContext, registry: &ToolRegistry) -> String {
    let soul_block = haily_kms::system_prompt::build(ctx);
    let tool_block = tool_reference_block(registry);

    format!(
        "{soul_block}

## Tool Calling
Khi cần dùng tool, output ĐÚNG format này (không có text nào trước hoặc sau):
<tool_call>{{\"tool\":\"name\",\"args\":{{...}}}}</tool_call>

Sau khi nhận tool result, tiếp tục trả lời bình thường.

## Delegation Strategy
- **Quick tasks** (tra cứu nhanh, nhắc nhở, ghi chú, check lịch): dùng quick tools trực tiếp.
- **Domain tasks** (kỹ thuật, nghiên cứu, tài chính, v.v.): gọi delegate_to_<domain> với task mô tả rõ yêu cầu.
- **Compound requests** (yêu cầu gồm 2-3 việc khác nhau): gọi delegate tools **tuần tự** — từng bước một, dùng kết quả bước trước làm context cho bước sau.

Ví dụ: yêu cầu nghiên cứu về ETF rồi lên kế hoạch tiết kiệm
  Bước 1: delegate_to_researcher với task = nghiên cứu ETF index fund phù hợp cho nhà đầu tư mới
  Bước 2: delegate_to_finance với task = lên kế hoạch tiết kiệm đầu tư ETF, context = kết quả từ bước 1

## Available Tools
{tool_block}"
    )
}

/// Load recent messages from DB and convert to LLM message format.
/// `window` = max number of past turns to include (user+assistant pairs).
pub async fn load_history(
    db: &haily_db::DbHandle,
    session_id: &str,
    window: i64,
) -> anyhow::Result<Vec<Message>> {
    let msgs = sessions::recent_messages(db, session_id, window * 2).await?;
    Ok(msgs
        .into_iter()
        .map(|m| Message {
            role: match m.role.as_str() {
                "assistant" => haily_llm::Role::Assistant,
                "system" => haily_llm::Role::System,
                _ => haily_llm::Role::User,
            },
            content: m.content,
        })
        .collect())
}

/// Builds the messages array for the LLM call.
///
/// Order: system → history → (optional: memory preamble as user) → current user message
pub async fn build_messages(
    kms: &KmsHandle,
    db: &haily_db::DbHandle,
    registry: &ToolRegistry,
    session_id: &str,
    user_message: &str,
) -> anyhow::Result<(Vec<Message>, LifeContext)> {
    // Load identity + preferences
    let mut ctx = kms.build_life_context(uuid::Uuid::parse_str(session_id).unwrap_or_default()).await?;

    // Hybrid search to surface relevant facts for this message
    let search_results = kms.search_hybrid(user_message, 8).await.unwrap_or_default();
    ctx.relevant_facts = search_results.into_iter().map(|r| r.text).collect();

    let system_prompt = build_full_system_prompt(&ctx, registry);
    let history = load_history(db, session_id, 15).await?;

    let mut messages = vec![Message::system(system_prompt)];
    messages.extend(history);
    messages.push(Message::user(user_message));

    Ok((messages, ctx))
}
