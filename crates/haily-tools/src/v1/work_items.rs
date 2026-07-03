use crate::{RiskTier, Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use haily_db::queries::work_items;
use serde_json::{json, Value};

fn status_emoji(status: &str) -> &'static str {
    match status {
        "running"     => "🔄",
        "interrupted" => "⏸",
        "queued"      => "⏳",
        "done"        => "✅",
        "failed"      => "❌",
        _             => "❓",
    }
}

// ---------------------------------------------------------------------------
// WorkItemListTool
// ---------------------------------------------------------------------------
pub struct WorkItemListTool;

#[async_trait]
impl Tool for WorkItemListTool {
    fn name(&self) -> &str { "work_item_list" }

    fn description(&self) -> &str {
        "Liệt kê các công việc đang chạy hoặc bị dừng dang dở. \
         Dùng khi user hỏi 'đang làm gì', 'haily đang làm gì', 'việc nào dở dang'."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "enum": ["active", "interrupted", "all"],
                    "default": "active",
                    "description": "Bộ lọc: active (đang chạy/chờ/tạm dừng), interrupted (bị dừng), all (tất cả non-terminal)"
                }
            }
        })
    }

    fn risk_tier(&self, _args: &Value) -> RiskTier { RiskTier::Read }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let filter = args["filter"].as_str().unwrap_or("active");

        let items = match filter {
            "interrupted" => work_items::list_interrupted(&ctx.db).await?,
            // "all" and "active" both use list_active which covers all non-terminal statuses.
            _ => work_items::list_active(&ctx.db).await?,
        };

        if items.is_empty() {
            return Ok("Không có công việc nào đang chạy.".to_string());
        }

        let mut table = String::from("Danh sách công việc:\n");
        table.push_str(&format!(
            "{:<3} {:<40} {:<12} {:<8} {}\n",
            "", "Tiêu đề", "Trạng thái", "Tiến độ", "ID"
        ));
        table.push_str(&"-".repeat(80));
        table.push('\n');

        for item in &items {
            let emoji = status_emoji(&item.status);
            // Use char-based truncation to avoid panicking on multibyte UTF-8
            // boundaries (Vietnamese text has multi-byte diacritics).
            let title = if item.title.chars().count() > 38 {
                let truncated: String = item.title.chars().take(37).collect();
                format!("{truncated}…")
            } else {
                item.title.clone()
            };
            let phase = item.phase.as_deref().unwrap_or("—");
            table.push_str(&format!(
                "{:<3} {:<40} {:<12} {:>6}%  {} | bước: {}\n",
                emoji, title, item.status, item.progress, item.id, phase
            ));
        }

        Ok(table)
    }
}

// ---------------------------------------------------------------------------
// WorkItemResumeTool
// ---------------------------------------------------------------------------
pub struct WorkItemResumeTool;

#[async_trait]
impl Tool for WorkItemResumeTool {
    fn name(&self) -> &str { "work_item_resume" }

    fn description(&self) -> &str {
        "Xem chi tiết và checkpoint của một công việc bị dừng. \
         Dùng khi user muốn tiếp tục việc dang dở."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Work item ID"
                }
            },
            "required": ["id"]
        })
    }

    fn risk_tier(&self, _args: &Value) -> RiskTier { RiskTier::ReversibleWrite }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let id = args["id"].as_str().ok_or_else(|| anyhow::anyhow!("id required"))?;

        let Some(item) = work_items::get(&ctx.db, id).await? else {
            return Ok(format!("Không tìm thấy work item với id: {id}"));
        };

        let phase = item.phase.as_deref().unwrap_or("(chưa bắt đầu)");
        let started = item.started_at.as_deref().unwrap_or("N/A");
        let checkpoint = item
            .checkpoint
            .as_deref()
            .unwrap_or("Không có dữ liệu checkpoint");

        Ok(format!(
            "📋 Công việc: {title}\n\
             Trạng thái: {status}\n\
             Bước cuối: {phase}\n\
             Tiến độ: {progress}%\n\
             Bắt đầu: {started}\n\
             \n\
             Checkpoint: {checkpoint}\n\
             \n\
             💡 Nhắn lại yêu cầu gốc để tiếp tục từ đầu, hoặc mô tả bạn muốn tiếp tục từ đâu.",
            title    = item.title,
            status   = item.status,
            phase    = phase,
            progress = item.progress,
            started  = started,
            checkpoint = checkpoint,
        ))
    }
}
