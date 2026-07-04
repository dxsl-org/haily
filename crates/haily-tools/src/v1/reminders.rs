use super::set_last_journal_id;
use crate::connector::redact;
use crate::{RiskTier, Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use haily_db::queries::local_snapshot::{local_journaled_write, LocalMutation};
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

        // The id is minted here (not by `reminders::insert`) because the journal outbox row
        // and the forward INSERT must reference the SAME id inside one transaction (C2).
        let id = uuid::Uuid::new_v4().to_string();
        let request_params = redact::redact_to_string(args.clone(), "local");
        let outcome = local_journaled_write(
            &ctx.db,
            LocalMutation::ReminderAdd {
                id: &id,
                title,
                fire_at,
                recurrence,
                session_id: &session_id,
            },
            &session_id,
            "reminder_add",
            "ReversibleWrite",
            &request_params,
            Some(&ctx.turn_id.to_string()),
            crate::LOCAL_RETENTION_DAYS,
        )
        .await?;
        set_last_journal_id(ctx, outcome.as_ref());
        Ok(format!("Đã đặt nhắc nhở: \"{title}\" vào {fire_at} (id: {id})"))
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
            LocalMutation::ReminderDelete { id },
            &ctx.session_id.to_string(),
            "reminder_delete",
            "ReversibleWrite",
            &request_params,
            Some(&ctx.turn_id.to_string()),
            crate::LOCAL_RETENTION_DAYS,
        )
        .await?;
        set_last_journal_id(ctx, outcome.as_ref());
        Ok(if outcome.is_some() {
            format!("Đã hủy nhắc nhở id={id}.")
        } else {
            format!("Không tìm thấy nhắc nhở id={id}.")
        })
    }
}
