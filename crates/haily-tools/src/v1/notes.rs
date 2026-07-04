use super::set_last_journal_id;
use crate::connector::redact;
use crate::{RiskTier, Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use haily_db::queries::local_snapshot::{local_journaled_write, LocalMutation};
use haily_db::queries::notes;
use serde_json::{json, Value};

/// Extract [[wikilink]] targets from note content.
fn extract_wikilinks(content: &str) -> String {
    let mut links = Vec::new();
    let mut remaining = content;
    while let Some(start) = remaining.find("[[") {
        remaining = &remaining[start + 2..];
        if let Some(end) = remaining.find("]]") {
            links.push(remaining[..end].trim().to_string());
            remaining = &remaining[end + 2..];
        } else {
            break;
        }
    }
    links.join(",")
}

// ---------------------------------------------------------------------------
// NoteSaveTool
// ---------------------------------------------------------------------------
pub struct NoteSaveTool;

#[async_trait]
impl Tool for NoteSaveTool {
    fn name(&self) -> &str {
        "note_save"
    }
    fn description(&self) -> &str {
        "Lưu note mới. Tự động extract [[wikilinks]] từ nội dung."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title":   { "type": "string" },
                "content": { "type": "string" },
                "tags":    { "type": "string", "description": "Comma-separated tags" }
            },
            "required": ["title", "content"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let title = args["title"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("title required"))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("content required"))?;
        let tags = args["tags"].as_str();
        let wikilinks = extract_wikilinks(content);
        let wikilinks = if wikilinks.is_empty() {
            None
        } else {
            Some(wikilinks.as_str())
        };

        // The id is minted here (not by `notes::insert`) because the journal outbox row and
        // the forward INSERT must reference the SAME id inside one transaction (C2).
        let id = uuid::Uuid::new_v4().to_string();
        let request_params = redact::redact_to_string(args.clone(), "local");
        let outcome = local_journaled_write(
            &ctx.db,
            LocalMutation::NoteSave {
                id: &id,
                title,
                content,
                tags,
                wikilinks,
            },
            &ctx.session_id.to_string(),
            "note_save",
            "ReversibleWrite",
            &request_params,
            Some(&ctx.turn_id.to_string()),
            crate::LOCAL_RETENTION_DAYS,
        )
        .await?;
        set_last_journal_id(ctx, outcome.as_ref());
        Ok(format!("Đã lưu note: \"{title}\" (id: {id})"))
    }
}

// ---------------------------------------------------------------------------
// NoteSearchTool
// ---------------------------------------------------------------------------
pub struct NoteSearchTool;

#[async_trait]
impl Tool for NoteSearchTool {
    fn name(&self) -> &str {
        "note_search"
    }
    fn description(&self) -> &str {
        "Tìm kiếm notes theo từ khóa. Hỗ trợ tìm full-text trong tiêu đề và nội dung."
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
        let limit = args["limit"].as_i64().unwrap_or(10);

        let results = notes::search_fts(&ctx.db, query, limit).await?;
        if results.is_empty() {
            return Ok(format!("Không tìm thấy note nào cho: {query}"));
        }

        let items: Vec<Value> = results
            .iter()
            .map(|n| {
                json!({
                    "id": n.id,
                    "title": n.title,
                    "tags": n.tags,
                    "snippet": n.content.chars().take(200).collect::<String>(),
                    "created_at": n.created_at
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&items)?)
    }
}

// ---------------------------------------------------------------------------
// NoteUpdateTool
// ---------------------------------------------------------------------------
pub struct NoteUpdateTool;

#[async_trait]
impl Tool for NoteUpdateTool {
    fn name(&self) -> &str {
        "note_update"
    }
    fn description(&self) -> &str {
        "Cập nhật nội dung của một note theo ID."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id":      { "type": "string" },
                "title":   { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["id", "title", "content"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("id required"))?;
        let title = args["title"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("title required"))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("content required"))?;

        let request_params = redact::redact_to_string(args.clone(), "local");
        let outcome = local_journaled_write(
            &ctx.db,
            LocalMutation::NoteUpdate { id, title, content },
            &ctx.session_id.to_string(),
            "note_update",
            "ReversibleWrite",
            &request_params,
            Some(&ctx.turn_id.to_string()),
            crate::LOCAL_RETENTION_DAYS,
        )
        .await?;
        set_last_journal_id(ctx, outcome.as_ref());
        Ok(if outcome.is_some() {
            format!("Đã cập nhật note id={id}.")
        } else {
            format!("Không tìm thấy note id={id}.")
        })
    }
}

// ---------------------------------------------------------------------------
// NoteDeleteTool
// ---------------------------------------------------------------------------
pub struct NoteDeleteTool;

#[async_trait]
impl Tool for NoteDeleteTool {
    fn name(&self) -> &str {
        "note_delete"
    }
    fn description(&self) -> &str {
        "Xóa note theo ID."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" }
            },
            "required": ["id"]
        })
    }
    /// Re-tiered `ReversibleWrite` (Harness Completion phase 2) — see the safety-net
    /// rationale on `RiskTier::ReversibleWrite`.
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("id required"))?;
        let request_params = redact::redact_to_string(args.clone(), "local");
        let outcome = local_journaled_write(
            &ctx.db,
            LocalMutation::NoteDelete { id },
            &ctx.session_id.to_string(),
            "note_delete",
            "ReversibleWrite",
            &request_params,
            Some(&ctx.turn_id.to_string()),
            crate::LOCAL_RETENTION_DAYS,
        )
        .await?;
        set_last_journal_id(ctx, outcome.as_ref());
        Ok(if outcome.is_some() {
            format!("Đã xóa note id={id}.")
        } else {
            format!("Không tìm thấy note id={id}.")
        })
    }
}
