use crate::{RiskTier, Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use haily_db::queries::facts;
use haily_kms::feedback::{self, FeedbackSignal};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// MemoryRememberTool
// ---------------------------------------------------------------------------
pub struct MemoryRememberTool;

#[async_trait]
impl Tool for MemoryRememberTool {
    fn name(&self) -> &str {
        "memory_remember"
    }
    fn description(&self) -> &str {
        "Lưu fact vào long-term memory. Dùng khi user chia sẻ thông tin quan trọng về bản thân."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject":   { "type": "string", "description": "Chủ thể (ví dụ: 'user', 'user.work')" },
                "predicate": { "type": "string", "description": "Quan hệ (ví dụ: 'thích', 'làm việc tại')" },
                "object":    { "type": "string", "description": "Giá trị (ví dụ: 'cà phê đen', 'startup XYZ')" },
                "domain":    { "type": "string", "description": "Domain phân loại (ví dụ: 'personal', 'work', 'health')" }
            },
            "required": ["subject", "predicate", "object"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let subject = args["subject"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("subject required"))?;
        let predicate = args["predicate"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("predicate required"))?;
        let object = args["object"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("object required"))?;
        let domain = args["domain"].as_str().unwrap_or("general");
        let session = ctx.session_id.to_string();

        let id = ctx
            .kms
            .remember(domain, subject, predicate, object, &session, None)
            .await?;
        Ok(format!(
            "Đã nhớ: \"{subject} {predicate} {object}\" (id: {id})"
        ))
    }
}

// ---------------------------------------------------------------------------
// MemorySearchTool
// ---------------------------------------------------------------------------
pub struct MemorySearchTool;

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }
    fn description(&self) -> &str {
        "Tìm kiếm trong long-term memory bằng ngôn ngữ tự nhiên (hybrid semantic + keyword)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "description": "Số kết quả tối đa (default 10)" }
            },
            "required": ["query"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("query required"))?;
        let limit = args["limit"].as_u64().unwrap_or(10) as usize;

        let results = ctx.kms.search_hybrid(query, limit).await?;
        if results.is_empty() {
            return Ok(format!("Không tìm thấy memory nào cho: {query}"));
        }

        let items: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "text": r.text,
                    "score": r.score,
                    "source": format!("{:?}", r.source)
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&items)?)
    }
}

// ---------------------------------------------------------------------------
// MemoryListTool
// ---------------------------------------------------------------------------
pub struct MemoryListTool;

#[async_trait]
impl Tool for MemoryListTool {
    fn name(&self) -> &str {
        "memory_list"
    }
    fn description(&self) -> &str {
        "Liệt kê facts trong memory. Dùng khi user hỏi 'mày biết gì về tao?'"
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "domain": { "type": "string", "description": "Lọc theo domain (optional)" },
                "limit":  { "type": "integer", "description": "Số kết quả tối đa (default 20)" }
            }
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let limit = args["limit"].as_i64().unwrap_or(20);

        let rows = if let Some(domain) = args["domain"].as_str() {
            facts::list_by_domain(&ctx.db, domain, limit).await?
        } else {
            facts::list_top(&ctx.db, limit).await?
        };

        if rows.is_empty() {
            return Ok("Memory chưa có facts nào.".to_string());
        }

        let items: Vec<Value> = rows
            .iter()
            .map(|f| {
                json!({
                    "id": f.id,
                    "domain": f.domain_id,
                    "fact": format!("{} {} {}", f.subject, f.predicate, f.object),
                    "confidence": f.confidence
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&items)?)
    }
}

// ---------------------------------------------------------------------------
// FeedbackReactTool — explicit 👍/👎 or correction from the user
// ---------------------------------------------------------------------------
pub struct FeedbackReactTool;

#[async_trait]
impl Tool for FeedbackReactTool {
    fn name(&self) -> &str {
        "feedback_react"
    }
    fn description(&self) -> &str {
        "Ghi lại phản hồi rõ ràng của user về câu trả lời vừa rồi: tích cực, tiêu cực hoặc correction. \
         Haily gọi tool này khi user nói 👍/👎 hoặc sửa thông tin."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reaction": {
                    "type": "string",
                    "enum": ["positive", "negative", "correction"],
                    "description": "Loại phản hồi"
                },
                "about": {
                    "type": "string",
                    "description": "Khía cạnh bị phê bình (ví dụ: 'response_length', 'tone'). Tuỳ chọn."
                },
                "correction_old": {
                    "type": "string",
                    "description": "Thông tin cũ (khi reaction = correction)"
                },
                "correction_new": {
                    "type": "string",
                    "description": "Thông tin đúng (khi reaction = correction)"
                }
            },
            "required": ["reaction"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let reaction = args["reaction"].as_str().unwrap_or("positive");
        let signal = match reaction {
            "positive" => FeedbackSignal::Positive,
            "negative" => FeedbackSignal::Negative {
                topic: args["about"].as_str().map(str::to_string),
            },
            "correction" => {
                let old = args["correction_old"].as_str().unwrap_or("").to_string();
                let new = args["correction_new"].as_str().unwrap_or("").to_string();
                FeedbackSignal::Correction { old, new }
            }
            _ => FeedbackSignal::Positive,
        };

        // Explicit tool call (`feedback_react`) — the highest-confidence provenance
        // (m2): the user deliberately invoked this tool, not a phrase-matched guess.
        feedback::apply_feedback_signal(&signal, &ctx.db, &ctx.session_id.to_string(), true)
            .await?;
        Ok(format!("Đã ghi lại phản hồi: {reaction}"))
    }
}

// ---------------------------------------------------------------------------
// MemoryForgetTool
// ---------------------------------------------------------------------------
pub struct MemoryForgetTool;

#[async_trait]
impl Tool for MemoryForgetTool {
    fn name(&self) -> &str {
        "memory_forget"
    }
    fn description(&self) -> &str {
        "Xóa một fact khỏi memory theo ID. Dùng khi user muốn Haily quên thông tin."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "ID của fact cần xóa" }
            },
            "required": ["id"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::IrreversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("id required"))?;
        // Routes through `KmsHandle::forget_fact` (not `facts::soft_delete` directly)
        // so the fact is tombstoned out of ANN search in this same process — a bare
        // DB soft-delete would leave it reachable via HNSW until the next rebuild.
        if ctx.kms.forget_fact(id).await? {
            Ok(format!("Đã xóa fact id={id} khỏi memory."))
        } else {
            Ok(format!("Không tìm thấy fact id={id}."))
        }
    }
}
