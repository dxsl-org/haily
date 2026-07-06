//! C6/C7 startup reconciliation sweep. Classifies orphan `pending` journal rows left by
//! a crash mid-write (kill-switch thrown between the outbox insert and the post-write
//! read-back) by asking the connector to read the record back.
//!
//! NEVER blind-retries a create (Odoo has no idempotency — M4): reconciliation only
//! READS. An unknown external outcome maps to a `readback_status`, not a re-issue.
//!
//! C1 dispatch split: a LOCAL row (tasks/notes/reminders) is classified from a live SELECT
//! against this process's OWN SQLite connection, never via `ConnectorExecutor::read_back` —
//! calling the (possibly `UnconfiguredExecutor`) connector executor for a local row would
//! bail and misclassify it `unverified` forever. In practice a local write's transaction
//! (C2) means a row only ever reaches `pending` if the process crashed BEFORE the commit
//! that would have set it to `match` — in which case the row itself was rolled back and
//! never exists to be swept. This branch exists as the fail-closed backstop for that
//! invariant, not a normally-hit path.
use crate::connector::{readback_diff, redact, ConnectorExecutor};
use crate::journal_undo::local_compensator::{is_local_row, local_table_for};
use crate::journal_undo::logic::plan_target_id;
use crate::journal_undo::ConnectorResolver;
use haily_db::queries::local_snapshot;
use haily_db::{queries::journal, DbHandle};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// Grace window: a row inserted less than this many seconds ago is assumed to be a write
/// still legitimately in flight, not an orphan, and is skipped.
pub const RECONCILE_GRACE_SECS: i64 = 30;

/// Sweep all incomplete rows and classify each via a read-back GET. Returns the number
/// of rows whose `readback_status` was advanced off `pending`. The startup caller passes
/// `RECONCILE_GRACE_SECS`; tests pass a smaller (or negative) window to include freshly
/// inserted rows without waiting.
///
/// M5c: each non-local row resolves its OWN executor from `resolver` by `tool_name` (a
/// manifest op) — never a single shared executor regardless of which connector owns it. An
/// unresolvable op (unconfigured/removed connector) fails closed to `unverified`, same as
/// the old placeholder's behavior, just now decided per-row instead of globally.
///
/// M2: a row whose pinned `manifest_hash` no longer matches the CURRENT manifest hash for
/// its op is also `unverified` — the manifest changed/moved since the write, so a read-back
/// against its NEW location would say nothing trustworthy about the OLD write.
///
/// C3: once ONE row's read-back against a given executor comes back `unverified` (read-back
/// GET failure — see `classify_one`), every REMAINING row that resolves to the SAME executor
/// this sweep is marked `unverified` WITHOUT another network call. Without this, a single
/// unreachable connector host would serially burn `CONNECTOR_TIMEOUT_SECS` per orphan row
/// instead of once — exactly the boot-hang risk this phase's C3 fix targets (the sweep itself
/// now runs off the boot critical path; this short-circuit additionally bounds ITS OWN
/// duration once running).
///
/// Classification:
/// - read-back shows the record present, matching request_params fields → `match`
/// - read-back shows it present but diverging → `mismatch`
/// - read-back GET itself failed (C7 lost response / flaky GET), no executor resolved, or a
///   hash-pin mismatch (M2) → `unverified` (does NOT block a later undo)
/// - record absent / genuinely unknown outcome → `unknown`
pub async fn reconcile_incomplete(db: &DbHandle, resolver: &ConnectorResolver, grace_secs: i64) -> u64 {
    let rows = match journal::list_incomplete(db, grace_secs).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("reconcile: failed to list incomplete journal rows: {e:#}");
            return 0;
        }
    };
    let mut classified = 0u64;
    // C3: executors already observed unreachable THIS sweep, keyed by pointer identity —
    // see the function doc.
    let mut unreachable_executors: HashSet<usize> = HashSet::new();
    for row in rows {
        // C1: a LOCAL row is classified from a live SELECT on THIS process's own SQLite
        // connection — never via a `ConnectorExecutor`, which would bail (or simply query
        // the wrong system) and misclassify forever.
        let (status, secret_source): (_, Option<&Arc<dyn ConnectorExecutor>>) = if is_local_row(&row) {
            (classify_local(db, &row).await, None)
        } else {
            match resolver.executor(&row.tool_name) {
                None => (("unverified", None), None), // unowned/unconfigured op — fail-closed
                Some(_) if !resolver.hash_matches(&row.tool_name, row.manifest_hash.as_deref()) => {
                    (("unverified", None), None) // M2: manifest changed since this write
                }
                Some(exec) => {
                    let key = executor_identity(exec);
                    if unreachable_executors.contains(&key) {
                        (("unverified", None), None) // C3: already known unreachable
                    } else {
                        let result = classify_one(exec.as_ref(), &row).await;
                        if result.0 == "unverified" {
                            unreachable_executors.insert(key);
                        }
                        let has_body = result.1.is_some();
                        (result, if has_body { Some(exec) } else { None })
                    }
                }
            }
        };
        // M3 + C5: scrub the executor's currently-resolved secret (if any) alongside the
        // tag-strip before the post_state summary is persisted / can reach an LLM — a
        // reflected credential in a reconciliation read-back must not leak here any more
        // than in the live write path (`HttpConnectorTool::execute`). Only resolved when
        // there IS a body to summarize (a local row's body is always `None`), so a local
        // row's reconciliation never triggers a needless third-party credential lookup.
        let summary = match status.1 {
            Some(v) => {
                let secret = match secret_source {
                    Some(exec) => exec.resolved_secret().await,
                    None => None,
                };
                Some(summarize(&v, secret.as_deref()))
            }
            None => None,
        };
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

/// Stable per-sweep identity for an executor, used only to detect "the SAME executor
/// already failed a read-back this sweep" (C3) — never persisted, never compared across
/// sweeps. Casting the fat trait-object pointer to `*const ()` drops the vtable half,
/// leaving just the data address, which is stable for the lifetime of the `Arc`.
fn executor_identity(exec: &Arc<dyn ConnectorExecutor>) -> usize {
    Arc::as_ptr(exec) as *const () as usize
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

/// Classify a LOCAL orphan row by checking whether its target row still exists in THIS
/// process's own SQLite — no network call, no `ConnectorExecutor`. In practice
/// `local_journaled_write` (C2) sets `readback_status = 'match'` inside the SAME transaction
/// as the forward mutate, so a local row only ever reaches this sweep if the process crashed
/// between the transaction's commit and... nothing — a committed transaction cannot leave a
/// row `pending`. This exists as the fail-closed backstop: if a local row is ever found
/// `pending` (a future code path, a hand-crafted row), the target's existence is what decides
/// `match` (the row is there — the local write landed) vs. `unknown` (the row is gone — the
/// journal outbox insert landed but the forward mutate never committed, which cannot happen
/// under the current one-transaction design but is treated conservatively either way).
async fn classify_local(
    db: &DbHandle,
    row: &journal::ActionJournalRow,
) -> (&'static str, Option<Value>) {
    let table = match local_table_for(&row.tool_name) {
        Some(t) => t,
        None => return ("unknown", None),
    };
    match local_snapshot::row_exists(db, table, &row.correlation_ref).await {
        Ok(true) => ("match", None),
        Ok(false) => ("unknown", None),
        // A local read is against our own DB — a query failure is an infra fault, not a
        // remote-system ambiguity, but is still treated the same conservative way (C7 parity).
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

/// M3 + C5: scrub the resolved secret (if any) before truncating/tag-stripping, mirroring
/// `http_connector_tool::summarize` so the crash-recovery sweep and the live write path never
/// diverge on what counts as "safe to journal."
fn summarize(body: &Value, secret: Option<&str>) -> String {
    let raw = redact::redact_secret_value(&body.to_string(), secret);
    let trimmed: String = raw.chars().take(512).collect();
    redact::strip_tool_tags(&trimmed)
}
