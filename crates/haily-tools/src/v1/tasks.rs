use crate::connector::redact;
use crate::{RiskTier, Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use haily_db::queries::local_snapshot::{local_journaled_write, LocalMutation};
use haily_db::queries::tasks;
use serde_json::{json, Value};

/// Days a local-tool journal row's captured content is retained before purge — mirrors the
/// connector path's default (`CONNECTOR_RETENTION_DAYS`); local and connector rows share one
/// retention policy (USER-VALIDATED, phase 1 scope: no tier change).
const LOCAL_RETENTION_DAYS: i64 = crate::LOCAL_RETENTION_DAYS;

// ---------------------------------------------------------------------------
// TaskCreateTool
// ---------------------------------------------------------------------------
pub struct TaskCreateTool;

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "task_create"
    }
    fn description(&self) -> &str {
        "Tạo task mới. Dùng khi user muốn theo dõi việc cần làm."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title":       { "type": "string" },
                "priority":    { "type": "string", "enum": ["urgent","high","medium","low"], "default": "medium" },
                "due_at":      { "type": "string", "description": "RFC3339 deadline (optional)" },
                "description": { "type": "string" }
            },
            "required": ["title"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let title = args["title"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("title required"))?;
        let priority = args["priority"].as_str().unwrap_or("medium");
        let due_at = args["due_at"].as_str();
        let desc = args["description"].as_str();

        // The id is minted here (not by `tasks::insert`) because the journal outbox row and
        // the forward INSERT must reference the SAME id inside one transaction (C2).
        let id = uuid::Uuid::new_v4().to_string();
        let request_params = redact::redact_to_string(args.clone(), "local");
        local_journaled_write(
            &ctx.db,
            LocalMutation::TaskCreate {
                id: &id,
                title,
                description: desc,
                priority,
                due_at,
            },
            &ctx.session_id.to_string(),
            "task_create",
            "ReversibleWrite",
            &request_params,
            Some(&ctx.turn_id.to_string()),
            LOCAL_RETENTION_DAYS,
        )
        .await?;
        Ok(format!("Đã tạo task: \"{title}\" [{priority}] (id: {id})"))
    }
}

// ---------------------------------------------------------------------------
// TaskListTool
// ---------------------------------------------------------------------------
pub struct TaskListTool;

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }
    fn description(&self) -> &str {
        "Lấy danh sách tasks đang active (chưa done hoặc cancelled), theo thứ tự ưu tiên."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }

    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<String> {
        let active = tasks::active(&ctx.db).await?;
        if active.is_empty() {
            return Ok("Không có task nào đang active.".to_string());
        }

        let items: Vec<Value> = active
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "title": t.title,
                    "priority": t.priority,
                    "status": t.status,
                    "due_at": t.due_at,
                    "description": t.description
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&items)?)
    }
}

// ---------------------------------------------------------------------------
// TaskCompleteTool
// ---------------------------------------------------------------------------
pub struct TaskCompleteTool;

#[async_trait]
impl Tool for TaskCompleteTool {
    fn name(&self) -> &str {
        "task_complete"
    }
    fn description(&self) -> &str {
        "Đánh dấu task là done."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "ID của task" }
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
        let request_params = redact::redact_to_string(args.clone(), "local");
        let outcome = local_journaled_write(
            &ctx.db,
            LocalMutation::TaskComplete { id },
            &ctx.session_id.to_string(),
            "task_complete",
            "ReversibleWrite",
            &request_params,
            Some(&ctx.turn_id.to_string()),
            LOCAL_RETENTION_DAYS,
        )
        .await?;
        Ok(if outcome.is_some() {
            format!("Task id={id} đã được đánh dấu là done. ✓")
        } else {
            format!("Không tìm thấy task id={id}.")
        })
    }
}

// ---------------------------------------------------------------------------
// TaskDeleteTool
// ---------------------------------------------------------------------------
pub struct TaskDeleteTool;

#[async_trait]
impl Tool for TaskDeleteTool {
    fn name(&self) -> &str {
        "task_delete"
    }
    fn description(&self) -> &str {
        "Xóa task theo ID."
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
            LocalMutation::TaskDelete { id },
            &ctx.session_id.to_string(),
            "task_delete",
            "ReversibleWrite",
            &request_params,
            Some(&ctx.turn_id.to_string()),
            LOCAL_RETENTION_DAYS,
        )
        .await?;
        Ok(if outcome.is_some() {
            format!("Đã xóa task id={id}.")
        } else {
            format!("Không tìm thấy task id={id}.")
        })
    }
}
