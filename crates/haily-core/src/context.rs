/// Builds the LLM-ready message list and system prompt for each turn.
use crate::budget::{self, TokenBudget};
use haily_db::queries::sessions;
use haily_kms::{KmsHandle, LifeContext};
use haily_llm::Message;
use haily_tools::ToolRegistry;

/// `load_history` fetches this many DB rows (not LLM messages) regardless of budget —
/// cheap at the DB layer, and generous enough that the token budgeter (not the SQL
/// LIMIT) is what decides how much history actually reaches the model. Replaces the
/// old fixed 15-turn (30-message) window that was enforced with zero token counting.
const HISTORY_FETCH_WINDOW: i64 = 60;

/// Facts-trim rule (phase-05 spec): if the system prompt alone — with the FULL fact
/// list rendered — would already consume more than half the prompt budget, drop
/// facts beyond the top-3 (KMS returns them ranked by relevance, so the top-3 are the
/// most relevant) before rendering the prompt for real. A bloated Memory section
/// otherwise starves history/current-turn budget for a benefit (marginal fact #7)
/// that's rarely worth it.
const FACTS_TRIM_BUDGET_FRACTION: f64 = 0.5;
const FACTS_TRIM_KEEP_TOP_N: usize = 3;

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
///
/// Fetches a fixed, generous window (`HISTORY_FETCH_WINDOW` DB rows) — deliberately
/// NOT sized to any token budget. The DB fetch stays cheap and simple; deciding how
/// much of this actually reaches the model is `TokenBudget::fit_messages`'s job, so
/// the trimming logic lives in exactly one place instead of being duplicated at the
/// query layer too.
pub async fn load_history(
    db: &haily_db::DbHandle,
    session_id: &str,
) -> anyhow::Result<Vec<Message>> {
    let msgs = sessions::recent_messages(db, session_id, HISTORY_FETCH_WINDOW).await?;
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

/// Renders the system prompt, applying the facts-trim rule if the full fact list
/// would blow the facts-trim threshold on its own. Returns the final prompt text.
///
/// `ctx.relevant_facts` is truncated in place to the top-3 (KMS ranks by relevance,
/// so truncation keeps the most relevant ones) when the FULL rendering exceeds
/// `FACTS_TRIM_BUDGET_FRACTION` of the prompt budget — cheap to check because
/// `build_full_system_prompt` is a pure string builder with no I/O.
fn build_trimmed_system_prompt(
    ctx: &mut LifeContext,
    registry: &ToolRegistry,
    prompt_budget: usize,
) -> String {
    let full_prompt = build_full_system_prompt(ctx, registry);
    let threshold = (prompt_budget as f64 * FACTS_TRIM_BUDGET_FRACTION) as usize;

    if budget::estimate(&full_prompt) <= threshold || ctx.relevant_facts.len() <= FACTS_TRIM_KEEP_TOP_N {
        return full_prompt;
    }

    tracing::debug!(
        fact_count = ctx.relevant_facts.len(),
        "system prompt exceeds facts-trim threshold — keeping top-3 facts"
    );
    ctx.relevant_facts.truncate(FACTS_TRIM_KEEP_TOP_N);
    build_full_system_prompt(ctx, registry)
}

/// Builds the messages array for the LLM call, fitted to the active backend's token
/// budget (`context_window`).
///
/// Order: system → (budget-trimmed) history → current user message. History is
/// trimmed oldest-first by `TokenBudget::fit_messages`; the system message and the
/// current user message are always pinned (see `budget.rs`'s pinning rule).
pub async fn build_messages(
    kms: &KmsHandle,
    db: &haily_db::DbHandle,
    registry: &ToolRegistry,
    session_id: &str,
    user_message: &str,
    context_window: u32,
) -> anyhow::Result<(Vec<Message>, LifeContext)> {
    // Load identity + preferences. A malformed session_id must fail loudly here —
    // silently substituting Uuid::nil() (the old `unwrap_or_default()` behavior) would
    // route this turn's memory/preferences lookups to the wrong (nil) identity.
    let parsed_session_id = uuid::Uuid::parse_str(session_id)
        .map_err(|e| anyhow::anyhow!("invalid session_id '{session_id}': {e}"))?;
    let mut ctx = kms.build_life_context(parsed_session_id).await?;

    // Hybrid search to surface relevant facts for this message
    let search_results = kms.search_hybrid(user_message, 8).await.unwrap_or_default();
    ctx.relevant_facts = search_results.into_iter().map(|r| r.text).collect();

    let token_budget = TokenBudget::new(context_window);
    let system_prompt_text =
        build_trimmed_system_prompt(&mut ctx, registry, token_budget.prompt_budget());
    let system_message = Message::system(system_prompt_text);

    let history = load_history(db, session_id).await?;
    let current_turn = [Message::user(user_message)];

    let messages = token_budget.fit_messages(&system_message, &history, &current_turn);

    Ok((messages, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_kms::Soul;

    fn ctx_with_facts(relevant_facts: Vec<String>) -> LifeContext {
        LifeContext {
            agent_name: "Haily".to_string(),
            soul: Soul::Haily,
            user_address: "bạn".to_string(),
            agent_pronoun: "mình".to_string(),
            relevant_facts,
            feedback_directives: vec![],
            active_skills: vec![],
        }
    }

    #[test]
    fn facts_trim_leaves_small_fact_list_untouched() {
        let mut ctx = ctx_with_facts(vec!["fact one".to_string(), "fact two".to_string()]);
        let registry = ToolRegistry::new();
        // Generous budget: nowhere near the 50% threshold.
        let prompt = build_trimmed_system_prompt(&mut ctx, &registry, 100_000);

        assert_eq!(ctx.relevant_facts.len(), 2, "small fact lists must not be trimmed");
        assert!(prompt.contains("fact one"));
        assert!(prompt.contains("fact two"));
    }

    #[test]
    fn facts_trim_keeps_only_top_3_when_full_prompt_exceeds_half_budget() {
        // 50 long facts guarantee the rendered system prompt exceeds 50% of even a
        // generous budget.
        let facts: Vec<String> = (0..50)
            .map(|i| format!("fact-{i}-{}", "x".repeat(200)))
            .collect();
        let mut ctx = ctx_with_facts(facts);
        let registry = ToolRegistry::new();

        // A tight budget relative to the 50-fact prompt forces the trim path.
        let prompt = build_trimmed_system_prompt(&mut ctx, &registry, 2000);

        assert_eq!(ctx.relevant_facts.len(), FACTS_TRIM_KEEP_TOP_N, "must trim to top-3");
        assert!(prompt.contains("fact-0-"), "top-ranked fact (index 0) must survive");
        assert!(prompt.contains("fact-2-"), "third-ranked fact must survive");
        assert!(!prompt.contains("fact-49-"), "low-ranked facts must be dropped");
    }

    #[test]
    fn facts_trim_never_drops_below_top_3_even_if_still_over_threshold() {
        // Even after trimming to 3, a pathologically huge budget deficit can't shrink
        // further — 3 is the floor, not renegotiated based on remaining overage.
        let facts: Vec<String> = (0..10).map(|i| format!("fact-{i}")).collect();
        let mut ctx = ctx_with_facts(facts);
        let registry = ToolRegistry::new();

        let prompt = build_trimmed_system_prompt(&mut ctx, &registry, 1);

        assert_eq!(ctx.relevant_facts.len(), FACTS_TRIM_KEEP_TOP_N);
        assert!(prompt.contains("fact-0"));
    }
}
