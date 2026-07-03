//! Undo state-machine logic, decoupled from the `Tool` trait so it is unit-testable
//! against a mock `ConnectorExecutor` without a `ToolContext`.
//!
//! Retry-safety (M4/M7): read-back BEFORE each compensation (skip if already at the
//! target state), `MissingError` on an unlink-compensation is treated as already-done
//! (not retryable), an UNRECOGNIZED faultString is non-retryable (fail-closed), and
//! `undo_attempts` is hard-capped at `MAX_UNDO_ATTEMPTS`. Retries are USER-initiated —
//! there is no background worker; a `stuck` row links its raw `compensation_plan` for
//! manual action.
use crate::connector::{redact, ConnectorExecutor, ExecOutcome};
use anyhow::Result;
use haily_db::{queries::journal, queries::journal::ActionJournalRow, DbHandle};
use serde_json::Value;

/// Hard cap on undo attempts (M4/M7). Beyond this the row is `stuck` for manual action.
pub const MAX_UNDO_ATTEMPTS: i64 = 3;

/// Structured fault codes that mean "the record is already gone" — an unlink/delete
/// compensation seeing this is ALREADY DONE, not a retryable failure.
const ALREADY_GONE_CODES: &[&str] = &["MissingError"];

/// Result of a single undo attempt, mapped to a user-facing summary by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoOutcome {
    /// Compensation succeeded and was verified by its own read-back.
    Undone,
    /// Refused up-front (never attempted a write). Carries the reason.
    Refused(String),
    /// A read-back showed the record already at the target state — no write needed.
    AlreadyDone,
    /// Compensation failed but MAY be retried (attempts left, retryable fault).
    Failed(String),
    /// Terminal failure — non-retryable fault, or the attempt cap was hit. Row is stuck.
    Stuck(String),
}

/// Apply the undo refusal rules against a freshly-read row. Returns `Some(reason)` if
/// undo must be refused BEFORE any external call, else `None`.
///
/// Refuses on: `compensability == "final"`, retention expired, a `mismatch` read-back
/// with no clean pre_state, or no compensation_plan recorded.
pub fn refusal_reason(row: &ActionJournalRow) -> Option<String> {
    if row.undo_status == "undone" {
        return Some("hành động này đã được hoàn tác trước đó".to_string());
    }
    if row.compensability == "final" {
        return Some("hành động này không thể hoàn tác (final)".to_string());
    }
    if row.compensation_plan.is_none() {
        return Some("không có kế hoạch bồi hoàn được ghi lại".to_string());
    }
    if retention_expired(&row.retention_expires_at) {
        return Some("bản ghi hoàn tác đã hết hạn lưu trữ".to_string());
    }
    if row.readback_status == "mismatch" && !has_clean_pre_state(row) {
        return Some("trạng thái ghi nhận không khớp và không có pre_state sạch".to_string());
    }
    None
}

fn retention_expired(retention_expires_at: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(retention_expires_at) {
        Ok(exp) => exp < chrono::Utc::now(),
        // Fail-closed: an unparseable retention timestamp is treated as expired.
        Err(_) => true,
    }
}

fn has_clean_pre_state(row: &ActionJournalRow) -> bool {
    row.pre_state
        .as_deref()
        .is_some_and(|s| !s.is_empty() && s != "null")
}

/// C10 version guard: re-read the live `write_date` via read-back and REFUSE if it has
/// changed since the row was recorded. Read-back checks shape; this checks concurrency.
///
/// Returns `Ok(true)` if versions match (safe to compensate), `Ok(false)` if they
/// diverged (must refuse), or `Err` only if the read-back itself failed.
pub async fn write_date_unchanged(
    executor: &dyn ConnectorExecutor,
    row: &ActionJournalRow,
) -> Result<bool> {
    // Prefer the POST-write version as the concurrency baseline: it is the write_date AS OF
    // our own forward write, so the undo refuses only on a THIRD-PARTY change beyond it. Fall
    // back to the pre-write version only when the post-write read-back never landed (a record
    // written but unverified). No version at all → nothing to compare (creates) → allow.
    let recorded = match row
        .post_state_version
        .as_deref()
        .or(row.pre_state_version.as_deref())
    {
        Some(v) => v,
        None => return Ok(true),
    };
    // Version re-check reads back by the ORIGINAL op name (a manifest op) — the executor
    // resolves the model from the manifest, so no compensation model hint is needed. The
    // compensation plan's target id is passed as the id locator: an UPDATE's row carries a
    // generated correlation_ref that was NEVER written into the record (only creates embed the
    // ref), so the record can only be re-read by its id.
    let plan: Value = row
        .compensation_plan
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);
    let id_hint = plan_target_id(&plan);
    let current = executor
        .read_back(&row.tool_name, &row.correlation_ref, None, id_hint.as_deref())
        .await?;
    let live = current
        .get("write_date")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(live == recorded)
}

/// Drive one undo attempt for `row` through `executor`, persisting each state transition.
///
/// Sequence: refusal rules → C10 version re-check → read-back-before-compensation
/// (skip-if-already-target) → compensate → OWN read-back → `undone`. Every terminal
/// state is persisted so a USER-initiated retry resumes from the recorded state.
pub async fn attempt_undo(
    db: &DbHandle,
    executor: &dyn ConnectorExecutor,
    row: &ActionJournalRow,
) -> Result<UndoOutcome> {
    if let Some(reason) = refusal_reason(row) {
        journal::advance_undo_status(db, &row.id, "refused").await?;
        return Ok(UndoOutcome::Refused(reason));
    }
    journal::advance_undo_status(db, &row.id, "undo_requested").await?;

    // C10: refuse if the record changed under us since we recorded it.
    match write_date_unchanged(executor, row).await {
        Ok(false) => {
            journal::advance_undo_status(db, &row.id, "refused").await?;
            return Ok(UndoOutcome::Refused(
                "bản ghi đã bị thay đổi kể từ khi ghi nhận (write_date)".to_string(),
            ));
        }
        Ok(true) => {}
        // A read-back failure here does NOT permanently block undo — mark unverified and
        // proceed to compensate (retry-safe: the compensation reads back again).
        Err(_) => {
            journal::set_readback(db, &row.id, "unverified", None).await?;
        }
    }

    let cap = journal::increment_undo_attempt(db, &row.id).await?;
    if cap > MAX_UNDO_ATTEMPTS {
        journal::advance_undo_status(db, &row.id, "stuck").await?;
        return Ok(UndoOutcome::Stuck(format!(
            "đã thử hoàn tác {MAX_UNDO_ATTEMPTS} lần không thành công — cần xử lý thủ công"
        )));
    }

    let plan: Value = row
        .compensation_plan
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);
    let comp_op = plan
        .get("op")
        .and_then(Value::as_str)
        .unwrap_or("compensate");
    // The compensation plan's model — passed to every compensation read-back so the executor
    // queries the CORRECT model for a bare op keyword (a `mail.activity` unlink must read back
    // `mail.activity`, not the manifest's first model). `None` for a legacy plan with no model.
    let comp_model = plan.get("model").and_then(Value::as_str);
    // The compensation plan's target id — passed as the read-back id locator so a compensation
    // whose model has no correlation field (e.g. `mail.activity`, no `ref`) is still found by
    // id. For a model WITH a correlation field the executor prefers the ref; the id is the
    // fallback. `None` for a plan carrying no concrete id (guarded above for target ops).
    let comp_id = plan_target_id(&plan);

    // Fail-closed target guard: a write/unlink/archive compensation MUST carry the concrete
    // record id it targets. A create journals its plan BEFORE the call (no id yet) and the
    // tool writes the returned id back post-call — but if that write-back never landed (lost
    // create, crash between call and write-back), the plan still has no id. Running
    // `write(null, {active:false})` / `unlink(null)` here would target NO record — or, on
    // Odoo, potentially EVERY record — so refuse BEFORE any external call rather than
    // compensate blind. Checked before `advance_undo_status(compensating)` so the row is not
    // left mid-transition on a refusal.
    if compensation_needs_target(comp_op) && !plan_has_target(&plan) {
        journal::advance_undo_status(db, &row.id, "refused").await?;
        return Ok(UndoOutcome::Refused(
            "kế hoạch bồi hoàn thiếu id bản ghi mục tiêu (create bị mất phản hồi) — từ chối để tránh ghi nhầm".to_string(),
        ));
    }

    journal::advance_undo_status(db, &row.id, "compensating").await?;

    // Read-back-before-compensation (M4/M7): if the record is already at the target
    // state (e.g. already unlinked), skip the write — Odoo has no idempotency, so a
    // second unlink of a gone record would fault.
    if let Ok(current) = executor
        .read_back(comp_op, &row.correlation_ref, comp_model, comp_id.as_deref())
        .await
    {
        if already_at_target(&plan, &current) {
            journal::set_readback(db, &row.id, "match", Some(&summarize(&current))).await?;
            journal::advance_undo_status(db, &row.id, "undone").await?;
            return Ok(UndoOutcome::AlreadyDone);
        }
    }

    match executor.call(comp_op, &plan).await {
        Ok(ExecOutcome::Ok { .. }) => {
            // OWN read-back is REQUIRED before declaring `undone` — a 200 is not proof.
            match executor
                .read_back(comp_op, &row.correlation_ref, comp_model, comp_id.as_deref())
                .await
            {
                Ok(after) => {
                    journal::set_readback(db, &row.id, "match", Some(&summarize(&after))).await?;
                    journal::advance_undo_status(db, &row.id, "undone").await?;
                    Ok(UndoOutcome::Undone)
                }
                Err(_) => {
                    // Compensation call returned 200 but we could not verify it — do NOT
                    // claim undone. Leave failed for a USER-initiated retry.
                    journal::advance_undo_status(db, &row.id, "compensation_failed").await?;
                    Ok(UndoOutcome::Failed(
                        "bồi hoàn được gửi nhưng chưa xác minh được — thử lại".to_string(),
                    ))
                }
            }
        }
        Ok(ExecOutcome::Fault {
            fault_string,
            code,
            name,
        }) => {
            classify_fault(
                db,
                row,
                comp_op,
                &fault_string,
                code.as_deref(),
                name.as_deref(),
            )
            .await
        }
        // Transport/timeout Err (C7): do NOT conclude failed. The compensation MAY have
        // landed; leave failed so a USER-initiated retry reads back first.
        Err(_) => {
            journal::advance_undo_status(db, &row.id, "compensation_failed").await?;
            Ok(UndoOutcome::Failed(
                "không liên lạc được với hệ thống — thử lại sau".to_string(),
            ))
        }
    }
}

/// Classify a structured server fault into a terminal or retryable undo outcome.
async fn classify_fault(
    db: &DbHandle,
    row: &ActionJournalRow,
    comp_op: &str,
    fault_string: &str,
    code: Option<&str>,
    name: Option<&str>,
) -> Result<UndoOutcome> {
    let matches_gone = code.is_some_and(|c| ALREADY_GONE_CODES.contains(&c))
        || name.is_some_and(|n| ALREADY_GONE_CODES.contains(&n));
    let is_unlink = comp_op == "unlink" || comp_op == "delete";

    // MissingError on an unlink = the record is already gone = already-done, NOT retryable.
    if matches_gone && is_unlink {
        journal::advance_undo_status(db, &row.id, "undone").await?;
        return Ok(UndoOutcome::AlreadyDone);
    }

    // A RECOGNIZED, retryable fault code could be added here; today the fail-closed rule
    // is: an unrecognized faultString is NON-retryable. Tag-strip before it reaches an
    // LLM summary (C5).
    let safe = redact::strip_tool_tags(fault_string);
    journal::advance_undo_status(db, &row.id, "stuck").await?;
    Ok(UndoOutcome::Stuck(format!(
        "lỗi không xác định từ hệ thống, không tự động thử lại: {safe}"
    )))
}

/// True when a compensation op writes to a specific record and therefore REQUIRES a target
/// id on its plan. `write`/`archive`/`unlink`/`delete` all mutate an identified record; a
/// non-mutating or unknown keyword (e.g. `compensate`, `none`) is not target-checked here
/// (it either does nothing or is caught elsewhere).
fn compensation_needs_target(comp_op: &str) -> bool {
    matches!(comp_op, "write" | "archive" | "unlink" | "delete")
}

/// The compensation plan's concrete record id as a string, for the read-back id locator:
/// the first element of `ids`, else a scalar `id`. `None` when the plan carries no id (a lost
/// create — guarded by `plan_has_target` before any compensation runs).
pub(crate) fn plan_target_id(plan: &Value) -> Option<String> {
    if let Some(first) = plan.get("ids").and_then(Value::as_array).and_then(|a| a.first()) {
        return first
            .as_i64()
            .map(|n| n.to_string())
            .or_else(|| first.as_str().map(str::to_string));
    }
    plan.get("id")
        .and_then(|v| v.as_i64().map(|n| n.to_string()).or_else(|| v.as_str().map(str::to_string)))
}

/// True when the compensation plan carries a concrete record target: a non-empty `ids`
/// array, or a scalar `id`. Absence means the create's returned id was never written back —
/// the caller must refuse rather than compensate a null target.
fn plan_has_target(plan: &Value) -> bool {
    let ids_present = plan
        .get("ids")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    let id_present = plan
        .get("id")
        .is_some_and(|v| !v.is_null() && v != &Value::String(String::new()));
    ids_present || id_present
}

/// True when the read-back shows the record already at the compensation's target state.
/// For an unlink/delete plan, that is a null/empty/`false` body (Odoo returns `[]`/`false`
/// for a gone id). For other ops the check is conservative (never falsely skip).
fn already_at_target(plan: &Value, current: &Value) -> bool {
    let op = plan.get("op").and_then(Value::as_str).unwrap_or_default();
    if op == "unlink" || op == "delete" {
        return current.is_null()
            || current == &Value::Bool(false)
            || current.as_array().is_some_and(|a| a.is_empty());
    }
    false
}

/// Compact, tag-stripped (C5) summary of a read-back body for the journal/LLM — never
/// the raw third-party body (may carry a live tag or be large).
fn summarize(body: &Value) -> String {
    let raw = body.to_string();
    let trimmed: String = raw.chars().take(512).collect();
    redact::strip_tool_tags(&trimmed)
}

/// Counts returned by a batch undo. Batch iterates server-side and is EXEMPT from the
/// per-turn loop guard (a batch is one logical op, not a runaway tool loop).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BatchCounts {
    pub undone: usize,
    pub failed: usize,
    pub not_attempted: usize,
}

/// Undo every row in `ids`, per-row try/catch, tallying undone/failed/not_attempted.
/// A row that cannot be loaded counts as `not_attempted`; a refusal/stuck/failed counts
/// as `failed`; `undone`/`already-done` count as `undone`.
pub async fn batch_undo(
    db: &DbHandle,
    executor: &dyn ConnectorExecutor,
    ids: &[String],
) -> BatchCounts {
    let mut counts = BatchCounts::default();
    for id in ids {
        let row = match journal::get_by_id(db, id).await {
            Ok(Some(r)) => r,
            _ => {
                counts.not_attempted += 1;
                continue;
            }
        };
        match attempt_undo(db, executor, &row).await {
            Ok(UndoOutcome::Undone) | Ok(UndoOutcome::AlreadyDone) => counts.undone += 1,
            Ok(_) => counts.failed += 1,
            Err(_) => counts.failed += 1,
        }
    }
    counts
}
