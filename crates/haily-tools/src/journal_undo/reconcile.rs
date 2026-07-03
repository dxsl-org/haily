//! C6/C7 startup reconciliation sweep. Classifies orphan `pending` journal rows left by
//! a crash mid-write (kill-switch thrown between the outbox insert and the post-write
//! read-back) by asking the connector to read the record back.
//!
//! NEVER blind-retries a create (Odoo has no idempotency — M4): reconciliation only
//! READS. An unknown external outcome maps to a `readback_status`, not a re-issue.
use crate::connector::{readback_diff, redact, ConnectorExecutor};
use crate::journal_undo::logic::plan_target_id;
use haily_db::{queries::journal, DbHandle};
use serde_json::Value;

/// Grace window: a row inserted less than this many seconds ago is assumed to be a write
/// still legitimately in flight, not an orphan, and is skipped.
pub const RECONCILE_GRACE_SECS: i64 = 30;

/// Sweep all incomplete rows and classify each via a read-back GET. Returns the number
/// of rows whose `readback_status` was advanced off `pending`. The startup caller passes
/// `RECONCILE_GRACE_SECS`; tests pass a smaller (or negative) window to include freshly
/// inserted rows without waiting.
///
/// Classification:
/// - read-back shows the record present, matching request_params fields → `match`
/// - read-back shows it present but diverging → `mismatch`
/// - read-back GET itself failed (C7 lost response / flaky GET) → `unverified` (does NOT
///   block a later undo)
/// - record absent / genuinely unknown outcome → `unknown`
pub async fn reconcile_incomplete(
    db: &DbHandle,
    executor: &dyn ConnectorExecutor,
    grace_secs: i64,
) -> u64 {
    let rows = match journal::list_incomplete(db, grace_secs).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("reconcile: failed to list incomplete journal rows: {e:#}");
            return 0;
        }
    };
    let mut classified = 0u64;
    for row in rows {
        let status = classify_one(executor, &row).await;
        // Tag-strip the post_state summary before it is persisted / can reach an LLM (C5).
        let summary = status.1.map(|v| summarize(&v));
        if journal::set_readback(db, &row.id, status.0, summary.as_deref())
            .await
            .is_ok()
        {
            classified += 1;
        }
    }
    if classified > 0 {
        tracing::info!(count = classified, "reconciled incomplete journal rows");
    }
    classified
}

/// Read back one row and return its terminal `readback_status` + optional body.
async fn classify_one(
    executor: &dyn ConnectorExecutor,
    row: &journal::ActionJournalRow,
) -> (&'static str, Option<Value>) {
    // Reconcile always reads back by the ORIGINAL op name (row.tool_name is a manifest op),
    // so the executor resolves the model from the manifest — no compensation model hint. The
    // compensation plan's target id (when present) is passed as the id locator so a row whose
    // model has no correlation field, or an update whose ref was never embedded, is still
    // located by id rather than falsely classified `unknown`.
    let id_hint = row
        .compensation_plan
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|plan| plan_target_id(&plan));
    match executor
        .read_back(&row.tool_name, &row.correlation_ref, None, id_hint.as_deref())
        .await
    {
        Ok(body) => {
            if record_absent(&body) {
                // The create's response was lost AND the record is not present — genuinely
                // unknown outcome. NEVER blind-retry the create (M4); leave for manual/undo.
                ("unknown", Some(body))
            } else if request_fields_present(&row.request_params, &body) {
                ("match", Some(body))
            } else {
                ("mismatch", Some(body))
            }
        }
        // C7: the read-back GET itself failed — do NOT conclude the write failed. Mark
        // unverified; this does not block a later undo.
        Err(_) => ("unverified", None),
    }
}

fn record_absent(body: &Value) -> bool {
    body.is_null() || body == &Value::Bool(false) || body.as_array().is_some_and(|a| a.is_empty())
}

/// Diff ONLY the fields present in request_params against the read-back body — a
/// server-added field (e.g. `create_date`) must not trigger a false `mismatch`. Delegates to
/// the shared [`readback_diff`] normalizer so a crash-recovery classification uses the SAME
/// representation-aware comparison as the post-write verify (no divergence between the paths).
fn request_fields_present(request_params: &str, body: &Value) -> bool {
    let req: Value = match serde_json::from_str(request_params) {
        Ok(v) => v,
        Err(_) => return true, // can't diff — do not claim mismatch
    };
    // Odoo writes live under a `values` object; fall back to the top-level object.
    let expected = req.get("values").unwrap_or(&req);
    readback_diff::request_fields_match(expected, body)
}

fn summarize(body: &Value) -> String {
    let raw = body.to_string();
    let trimmed: String = raw.chars().take(512).collect();
    redact::strip_tool_tags(&trimmed)
}
