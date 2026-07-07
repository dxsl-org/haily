use super::set_last_journal_id;
use crate::connector::redact;
use crate::{RiskTier, Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use haily_db::queries::calendar;
use haily_db::queries::local_snapshot::{local_journaled_write, LocalMutation};
use haily_db::recurrence::RecurrenceRule;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// CalendarListTool
// ---------------------------------------------------------------------------
pub struct CalendarListTool;

#[async_trait]
impl Tool for CalendarListTool {
    fn name(&self) -> &str {
        "calendar_list"
    }
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
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let now = chrono::Utc::now();
        let from = args["from"].as_str().unwrap_or("").to_string();
        let from = if from.is_empty() {
            now.to_rfc3339()
        } else {
            from
        };
        let to = args["to"].as_str().unwrap_or("").to_string();
        let to = if to.is_empty() {
            (now + chrono::Duration::days(7)).to_rfc3339()
        } else {
            to
        };

        let events = calendar::upcoming(&ctx.db, &from, &to).await?;
        if events.is_empty() {
            return Ok("Không có sự kiện nào trong khoảng thời gian này.".to_string());
        }

        let items: Vec<Value> = events
            .iter()
            .map(|e| {
                json!({
                    "id": e.id,
                    "title": e.title,
                    "start_at": e.start_at,
                    "end_at": e.end_at,
                    "location": e.location,
                    "description": e.description,
                    "all_day": e.all_day == 1
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&items)?)
    }
}

// ---------------------------------------------------------------------------
// CalendarAddTool
// ---------------------------------------------------------------------------
pub struct CalendarAddTool;

#[async_trait]
impl Tool for CalendarAddTool {
    fn name(&self) -> &str {
        "calendar_add"
    }
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
                "all_day":     { "type": "boolean" },
                "recurrence":  {
                    "type": "string",
                    "description": "Lặp lại: 'daily' | 'weekly' | 'weekly:<mon..sun>' | \
                        'monthly:<1..31>' | 'every:<N>d'",
                    "nullable": true
                }
            },
            "required": ["title", "start_at", "end_at"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let title = args["title"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("title required"))?;
        let start_at = args["start_at"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("start_at required"))?;
        let end_at = args["end_at"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("end_at required"))?;
        let desc = args["description"].as_str();
        let location = args["location"].as_str();
        let all_day = args["all_day"].as_bool().unwrap_or(false);
        let recurrence = args["recurrence"].as_str();
        // Validated up front against the SAME grammar `reminder_add`/the proactive daemon
        // enforce (`RecurrenceRule`, reused from `haily-db`, never forked) — a malformed
        // rule stored here would silently never expand in `calendar::upcoming`.
        if let Some(r) = recurrence {
            if RecurrenceRule::parse(r).is_none() {
                return Err(anyhow::anyhow!(
                    "recurrence rule not supported: '{r}' (supported: daily, weekly, \
                     weekly:<mon..sun>, monthly:<1..31>, every:<N>d)"
                ));
            }
        }

        // The id is minted here (not by the forward INSERT) because the journal outbox row
        // and the forward INSERT must reference the SAME id inside one transaction (C2).
        let id = uuid::Uuid::new_v4().to_string();
        let request_params = redact::redact_to_string(args.clone(), "local");
        let outcome = local_journaled_write(
            &ctx.db,
            LocalMutation::CalendarAdd {
                id: &id,
                title,
                description: desc,
                location,
                start_at,
                end_at,
                all_day,
                recurrence,
            },
            &ctx.session_id.to_string(),
            "calendar_add",
            "ReversibleWrite",
            &request_params,
            Some(&ctx.turn_id.to_string()),
            crate::LOCAL_RETENTION_DAYS,
        )
        .await?;
        set_last_journal_id(ctx, outcome.as_ref());
        Ok(format!("Đã tạo sự kiện: {title} (id: {id})"))
    }
}

// ---------------------------------------------------------------------------
// CalendarDeleteTool
// ---------------------------------------------------------------------------
/// `scope` distinguishes deleting one expanded occurrence of a recurring event (records
/// an exception — the series row is untouched) from deleting the whole series
/// (soft-deletes the `calendar_events` row, exactly like `task_delete`). Journaled under
/// TWO distinct internal tool_name strings (`calendar_delete_series`/
/// `calendar_delete_occurrence`) so `journal_undo::local_compensator::op_kind` can invert
/// each correctly without inspecting `pre_state` — the PUBLIC tool name stays
/// `"calendar_delete"` either way (a single LLM-facing tool, both scopes).
///
/// Re-tiered `ReversibleWrite` (Phase 13b, assistant-depth): safe ONLY because BOTH scopes
/// route through `local_journaled_write`/the compensator's `LocalOpKind::Delete` and
/// `LocalOpKind::DeleteOccurrence` arms, AND `"calendar_delete"` is listed in
/// `haily-core::tool_call::RETIERED_DELETE_TOOLS` (C1) — both landed in this SAME change.
pub struct CalendarDeleteTool;

#[async_trait]
impl Tool for CalendarDeleteTool {
    fn name(&self) -> &str {
        "calendar_delete"
    }
    fn description(&self) -> &str {
        "Xóa sự kiện lịch theo ID. `scope='occurrence'` xóa MỘT lần lặp cụ thể (cần \
         `occurrence_start`); `scope='series'` (mặc định) xóa toàn bộ chuỗi sự kiện."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "ID của sự kiện cần xóa" },
                "scope": {
                    "type": "string",
                    "enum": ["occurrence", "series"],
                    "default": "series",
                    "description": "'occurrence' xóa một lần lặp; 'series' xóa cả chuỗi"
                },
                "occurrence_start": {
                    "type": "string",
                    "description": "RFC3339 start_at của lần lặp cần xóa — bắt buộc khi scope='occurrence'"
                }
            },
            "required": ["id"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("id required"))?;
        // Fail closed on an unrecognized scope rather than silently falling back to
        // 'series' — a typo'd scope must never widen a single-occurrence intent into a
        // whole-series delete.
        let scope = match args["scope"].as_str() {
            None | Some("series") => "series",
            Some("occurrence") => "occurrence",
            Some(other) => {
                return Err(anyhow::anyhow!(
                    "invalid scope '{other}' (expected 'occurrence' or 'series')"
                ))
            }
        };
        let request_params = redact::redact_to_string(args.clone(), "local");

        if scope == "occurrence" {
            let occurrence_start = args["occurrence_start"].as_str().ok_or_else(|| {
                anyhow::anyhow!("occurrence_start required when scope='occurrence'")
            })?;
            // A friendly "not found" instead of surfacing the calendar_exceptions FK
            // constraint violation an unknown/already-deleted event id would otherwise
            // trip inside the transaction.
            if calendar::get(&ctx.db, id).await?.is_none() {
                return Ok(format!("Không tìm thấy sự kiện id={id}."));
            }
            let outcome = local_journaled_write(
                &ctx.db,
                LocalMutation::CalendarDeleteOccurrence {
                    event_id: id,
                    occurrence_start,
                },
                &ctx.session_id.to_string(),
                "calendar_delete_occurrence",
                "ReversibleWrite",
                &request_params,
                Some(&ctx.turn_id.to_string()),
                crate::LOCAL_RETENTION_DAYS,
            )
            .await?;
            set_last_journal_id(ctx, outcome.as_ref());
            return Ok(if outcome.is_some() {
                format!("Đã xóa lần lặp lúc {occurrence_start} của sự kiện id={id}.")
            } else {
                format!("Lần lặp lúc {occurrence_start} của sự kiện id={id} đã được xóa trước đó.")
            });
        }

        let outcome = local_journaled_write(
            &ctx.db,
            LocalMutation::CalendarDeleteSeries { id },
            &ctx.session_id.to_string(),
            "calendar_delete_series",
            "ReversibleWrite",
            &request_params,
            Some(&ctx.turn_id.to_string()),
            crate::LOCAL_RETENTION_DAYS,
        )
        .await?;
        set_last_journal_id(ctx, outcome.as_ref());
        Ok(if outcome.is_some() {
            format!("Đã xóa sự kiện id={id}.")
        } else {
            format!("Không tìm thấy sự kiện id={id}.")
        })
    }
}
