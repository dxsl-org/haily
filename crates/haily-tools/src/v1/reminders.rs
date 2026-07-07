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
                "fire_at":    {
                    "type": "string",
                    "description": "RFC3339 thời điểm fire, hoặc cụm từ tự nhiên VN/EN \
                        (vd: 'mỗi thứ 2', 'hàng ngày 7h', '8h sáng mai', 'tomorrow 8am')"
                },
                "recurrence": {
                    "type": "string",
                    "description": "Lặp lại (chỉ dùng khi fire_at là RFC3339): \
                        'daily' | 'weekly' | 'weekly:<mon..sun>' | 'monthly:<1..31>' | 'every:<N>d'",
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
        let fire_at_input = args["fire_at"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("fire_at required"))?;
        let recurrence_input = args["recurrence"].as_str();

        // `fire_at` accepts either an explicit RFC3339 instant (recurrence taken from the
        // caller's own `recurrence` arg, validated against the SAME grammar the daemon
        // enforces) OR a natural-language VN/EN phrase (recurrence, if any, derived by the
        // parser itself — the arg is ignored in that branch, since the phrase IS the rule).
        let (fire_at, recurrence): (String, Option<String>) =
            match chrono::DateTime::parse_from_rfc3339(fire_at_input) {
                Ok(_) => {
                    if let Some(r) = recurrence_input {
                        if haily_db::recurrence::RecurrenceRule::parse(r).is_none() {
                            return Err(anyhow::anyhow!(
                                "recurrence rule not supported: '{r}' (supported: daily, \
                                 weekly, weekly:<mon..sun>, monthly:<1..31>, every:<N>d)"
                            ));
                        }
                    }
                    (fire_at_input.to_string(), recurrence_input.map(str::to_string))
                }
                Err(_) => match crate::schedule::parse_schedule(fire_at_input, chrono::Local::now()) {
                    Some((derived_fire_at, derived_rule)) => (derived_fire_at, derived_rule),
                    None => {
                        return Err(anyhow::anyhow!(
                            "could not understand fire_at '{fire_at_input}' — supply an \
                             RFC3339 timestamp or a recognized phrase (e.g. 'daily 7h', \
                             'every Monday', 'tomorrow 8am')"
                        ));
                    }
                },
            };

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
                fire_at: &fire_at,
                recurrence: recurrence.as_deref(),
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
