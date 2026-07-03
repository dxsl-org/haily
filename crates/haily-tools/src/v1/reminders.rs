use crate::{RiskTier, Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use haily_db::queries::reminders;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// ReminderAddTool
// ---------------------------------------------------------------------------
pub struct ReminderAddTool;

#[async_trait]
impl Tool for ReminderAddTool {
    fn name(&self) -> &str {
        "reminder_add"
    }
    fn description(&self) -> &str {
        "Đặt nhắc nhở mới. Haily sẽ tự động fire qua Telegram đúng giờ."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title":      { "type": "string", "description": "Nội dung nhắc nhở" },
                "fire_at":    { "type": "string", "description": "RFC3339 thời điểm fire" },
                "recurrence": {
                    "type": "string",
                    "description": "Lặp lại: 'daily' | 'weekly' | cron expression",
                    "nullable": true
                }
            },
            "required": ["title", "fire_at"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let title = args["title"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("title required"))?;
        let fire_at = args["fire_at"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("fire_at required"))?;
        let recurrence = args["recurrence"].as_str();
        let session_id = ctx.session_id.to_string();

        let reminder =
            reminders::insert(&ctx.db, title, fire_at, recurrence, Some(&session_id)).await?;
        Ok(format!(
            "Đã đặt nhắc nhở: \"{}\" vào {} (id: {})",
            reminder.title, reminder.fire_at, reminder.id
        ))
    }
}

// ---------------------------------------------------------------------------
// ReminderListTool
// ---------------------------------------------------------------------------
pub struct ReminderListTool;

#[async_trait]
impl Tool for ReminderListTool {
    fn name(&self) -> &str {
        "reminder_list"
    }
    fn description(&self) -> &str {
        "Liệt kê tất cả nhắc nhở chưa xóa, kể cả chưa fire."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }

    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<String> {
        let rows = reminders::list_all(&ctx.db).await?;

        if rows.is_empty() {
            return Ok("Không có nhắc nhở nào.".to_string());
        }

        let items: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "title": r.title,
                    "fire_at": r.fire_at,
                    "recurrence": r.recurrence,
                    "fired_at": r.fired_at,
                    "outcome": r.outcome
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&items)?)
    }
}

// ---------------------------------------------------------------------------
// ReminderDeleteTool
// ---------------------------------------------------------------------------
pub struct ReminderDeleteTool;

#[async_trait]
impl Tool for ReminderDeleteTool {
    fn name(&self) -> &str {
        "reminder_delete"
    }
    fn description(&self) -> &str {
        "Hủy nhắc nhở theo ID."
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
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::IrreversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("id required"))?;
        if reminders::soft_delete(&ctx.db, id).await? {
            Ok(format!("Đã hủy nhắc nhở id={id}."))
        } else {
            Ok(format!("Không tìm thấy nhắc nhở id={id}."))
        }
    }
}
