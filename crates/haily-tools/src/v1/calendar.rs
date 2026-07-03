use crate::{RiskTier, Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use haily_db::queries::calendar;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// CalendarListTool
// ---------------------------------------------------------------------------
pub struct CalendarListTool;

#[async_trait]
impl Tool for CalendarListTool {
    fn name(&self) -> &str { "calendar_list" }
    fn description(&self) -> &str {
        "Lấy danh sách sự kiện lịch trong khoảng thời gian. Dùng khi user hỏi về lịch sắp tới."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from": { "type": "string", "description": "RFC3339 start (default: now)" },
                "to":   { "type": "string", "description": "RFC3339 end (default: 7 days from now)" }
            }
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier { RiskTier::Read }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let now = chrono::Utc::now();
        let from = args["from"].as_str()
            .unwrap_or("")
            .to_string();
        let from = if from.is_empty() { now.to_rfc3339() } else { from };
        let to = args["to"].as_str()
            .unwrap_or("")
            .to_string();
        let to = if to.is_empty() {
            (now + chrono::Duration::days(7)).to_rfc3339()
        } else {
            to
        };

        let events = calendar::upcoming(&ctx.db, &from, &to).await?;
        if events.is_empty() {
            return Ok("Không có sự kiện nào trong khoảng thời gian này.".to_string());
        }

        let items: Vec<Value> = events.iter().map(|e| json!({
            "id": e.id,
            "title": e.title,
            "start_at": e.start_at,
            "end_at": e.end_at,
            "location": e.location,
            "description": e.description,
            "all_day": e.all_day == 1
        })).collect();
        Ok(serde_json::to_string_pretty(&items)?)
    }
}

// ---------------------------------------------------------------------------
// CalendarAddTool
// ---------------------------------------------------------------------------
pub struct CalendarAddTool;

#[async_trait]
impl Tool for CalendarAddTool {
    fn name(&self) -> &str { "calendar_add" }
    fn description(&self) -> &str {
        "Tạo sự kiện lịch mới. Dùng khi user muốn đặt cuộc hẹn, meeting, hoặc sự kiện."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title":       { "type": "string" },
                "start_at":    { "type": "string", "description": "RFC3339" },
                "end_at":      { "type": "string", "description": "RFC3339" },
                "description": { "type": "string" },
                "location":    { "type": "string" },
                "all_day":     { "type": "boolean" }
            },
            "required": ["title", "start_at", "end_at"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier { RiskTier::ReversibleWrite }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let title    = args["title"].as_str().ok_or_else(|| anyhow::anyhow!("title required"))?;
        let start_at = args["start_at"].as_str().ok_or_else(|| anyhow::anyhow!("start_at required"))?;
        let end_at   = args["end_at"].as_str().ok_or_else(|| anyhow::anyhow!("end_at required"))?;
        let desc     = args["description"].as_str();
        let location = args["location"].as_str();
        let all_day  = args["all_day"].as_bool().unwrap_or(false);

        let event = calendar::insert(
            &ctx.db,
            calendar::NewCalendarEvent {
                title,
                description: desc,
                location,
                start_at,
                end_at,
                all_day,
                recurrence: None,
            },
        )
        .await?;
        Ok(format!("Đã tạo sự kiện: {} (id: {})", event.title, event.id))
    }
}

// ---------------------------------------------------------------------------
// CalendarDeleteTool
// ---------------------------------------------------------------------------
pub struct CalendarDeleteTool;

#[async_trait]
impl Tool for CalendarDeleteTool {
    fn name(&self) -> &str { "calendar_delete" }
    fn description(&self) -> &str { "Xóa sự kiện lịch theo ID." }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "ID của sự kiện cần xóa" }
            },
            "required": ["id"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier { RiskTier::IrreversibleWrite }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let id = args["id"].as_str().ok_or_else(|| anyhow::anyhow!("id required"))?;
        if calendar::soft_delete(&ctx.db, id).await? {
            Ok(format!("Đã xóa sự kiện id={id}."))
        } else {
            Ok(format!("Không tìm thấy sự kiện id={id}."))
        }
    }
}
