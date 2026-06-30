use crate::{Tool, ToolClass, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
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
    fn name(&self) -> &str { "note_save" }
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
    fn approval_class(&self) -> ToolClass { ToolClass::AutoApprove }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let title   = args["title"].as_str().ok_or_else(|| anyhow::anyhow!("title required"))?;
        let content = args["content"].as_str().ok_or_else(|| anyhow::anyhow!("content required"))?;
        let tags    = args["tags"].as_str();
        let wikilinks = extract_wikilinks(content);

        let note = notes::insert(&ctx.db, title, content, tags, None, None).await?;

        if !wikilinks.is_empty() {
            notes::set_wikilinks(&ctx.db, &note.id, &wikilinks).await?;
        }

        Ok(format!("Đã lưu note: \"{}\" (id: {})", note.title, note.id))
    }
}

// ---------------------------------------------------------------------------
// NoteSearchTool
// ---------------------------------------------------------------------------
pub struct NoteSearchTool;

#[async_trait]
impl Tool for NoteSearchTool {
    fn name(&self) -> &str { "note_search" }
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
    fn approval_class(&self) -> ToolClass { ToolClass::AutoApprove }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let query = args["query"].as_str().ok_or_else(|| anyhow::anyhow!("query required"))?;
        let limit = args["limit"].as_i64().unwrap_or(10);

        let results = notes::search_fts(&ctx.db, query, limit).await?;
        if results.is_empty() {
            return Ok(format!("Không tìm thấy note nào cho: {query}"));
        }

        let items: Vec<Value> = results.iter().map(|n| json!({
            "id": n.id,
            "title": n.title,
            "tags": n.tags,
            "snippet": n.content.chars().take(200).collect::<String>(),
            "created_at": n.created_at
        })).collect();
        Ok(serde_json::to_string_pretty(&items)?)
    }
}

// ---------------------------------------------------------------------------
// NoteUpdateTool
// ---------------------------------------------------------------------------
pub struct NoteUpdateTool;

#[async_trait]
impl Tool for NoteUpdateTool {
    fn name(&self) -> &str { "note_update" }
    fn description(&self) -> &str { "Cập nhật nội dung của một note theo ID." }
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
    fn approval_class(&self) -> ToolClass { ToolClass::AutoApprove }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let id      = args["id"].as_str().ok_or_else(|| anyhow::anyhow!("id required"))?;
        let title   = args["title"].as_str().ok_or_else(|| anyhow::anyhow!("title required"))?;
        let content = args["content"].as_str().ok_or_else(|| anyhow::anyhow!("content required"))?;

        notes::update_content(&ctx.db, id, title, content).await?;
        Ok(format!("Đã cập nhật note id={id}."))
    }
}

// ---------------------------------------------------------------------------
// NoteDeleteTool
// ---------------------------------------------------------------------------
pub struct NoteDeleteTool;

#[async_trait]
impl Tool for NoteDeleteTool {
    fn name(&self) -> &str { "note_delete" }
    fn description(&self) -> &str { "Xóa note theo ID." }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" }
            },
            "required": ["id"]
        })
    }
    fn approval_class(&self) -> ToolClass { ToolClass::RequireApproval }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let id = args["id"].as_str().ok_or_else(|| anyhow::anyhow!("id required"))?;
        if notes::soft_delete(&ctx.db, id).await? {
            Ok(format!("Đã xóa note id={id}."))
        } else {
            Ok(format!("Không tìm thấy note id={id}."))
        }
    }
}
