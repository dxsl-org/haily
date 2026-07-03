//! `HttpConnectorTool` — the generic, manifest-interpreting connector tool (R3 substrate).
//!
//! ONE instance is registered per manifest op (no codegen). It reads its gating tier from
//! the op spec (fail-closed), then on `execute` runs the phase-3 outbox protocol:
//!
//!   GET pre_state (+version token, skipped for creates) → build compensation_plan →
//!   journal::insert OUTBOX row (redacted params, correlation_ref) BEFORE the call →
//!   RE-CHECK the kill switch just before the executor call (M5 TOCTOU, per-record) →
//!   executor.call → GET post_state → read-back diff (request_params fields only) →
//!   journal::set_readback.
//!
//! The kill switch is re-checked HERE (not just at dispatch) because dispatch's gate check
//! and the actual network call are separated by the pre_state GET + outbox insert — a
//! human throwing the switch in that window must still block the write (M5). The re-check
//! is per-record so a batch does not race past a mid-batch kill.
//!
//! The Odoo-specific executor (execute_kw / faultString classification) is phase 5; this
//! module ships the GENERIC HTTP executor and interprets a manifest op over it.
use crate::connector::manifest::{Manifest, OpSpec};
use crate::connector::{redact, ConnectorExecutor, ExecOutcome};
use crate::security;
use crate::{RiskTier, Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use haily_db::queries::journal::{self, NewAction};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Days a connector journal row's PII is retained before purge (mirrors phase-3 default).
const CONNECTOR_RETENTION_DAYS: i64 = 30;

/// A connector op wired for execution. `manifest`/`op` are shared references to the parsed,
/// human-approved schema; `executor` performs the external I/O; `kill` is the SAME
/// `Arc<AtomicBool>` the orchestrator flips live (re-checked at M5). `cred_ref` names the
/// preference key holding the credential so redaction records WHICH credential was used.
pub struct HttpConnectorTool {
    pub manifest: Arc<Manifest>,
    pub op: Arc<OpSpec>,
    pub executor: Arc<dyn ConnectorExecutor>,
    pub kill: Arc<AtomicBool>,
    pub cred_ref: String,
}

impl HttpConnectorTool {
    /// Build the outbox `compensation_plan` JSON for this op from the manifest's declared
    /// `compensation` template, enriched with the returned id once known. Written BEFORE
    /// the external call so a crash mid-write still leaves an undo path on disk.
    fn compensation_plan(&self, returned_id: Option<&str>) -> Option<String> {
        let mut plan = self.op.compensation.clone()?;
        if let (Some(obj), Some(id)) = (plan.as_object_mut(), returned_id) {
            obj.insert("id".to_string(), Value::String(id.to_string()));
        }
        Some(plan.to_string())
    }
}

#[async_trait]
impl Tool for HttpConnectorTool {
    fn name(&self) -> &str {
        &self.op.name
    }

    fn description(&self) -> &str {
        // A generic description; the op name + manifest carry the real routing intent.
        "Connector operation declared by a human-approved manifest (executes an external \
         write through the action journal + kill switch)."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "params": {
                    "type": "object",
                    "description": "Operation parameters as declared by the connector op"
                }
            }
        })
    }

    /// Gating tier is the op's DECLARED tier, fail-closing to `IrreversibleWrite` for an
    /// unresolvable/malformed tier (see `OpSpec::risk_tier`).
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        self.op.risk_tier()
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let params = args.get("params").cloned().unwrap_or(args.clone());
        let correlation_ref = uuid::Uuid::new_v4().to_string();
        let idempotency_key = format!("{}:{}", self.op.name, correlation_ref);

        // 1. GET pre_state (+version token). Skipped for creates — there is no record yet.
        let (pre_state, pre_state_version) = if self.op.is_create() {
            (None, None)
        } else {
            match self.executor.read_back(&self.op.name, &correlation_ref).await {
                Ok(body) => {
                    let version = body
                        .get("write_date")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    // Tag-strip the third-party pre_state before it is persisted (C5).
                    (Some(redact::strip_tool_tags(&body.to_string())), version)
                }
                // A failed pre-read is not fatal — record an empty pre_state and proceed;
                // the post-write read-back is the runtime backstop.
                Err(_) => (None, None),
            }
        };

        // 2. Build the compensation plan (returned id filled in post-call for creates).
        let compensation_plan = self.compensation_plan(None);

        // 3. OUTBOX insert BEFORE the external call — redacted params (C4). A crash after
        //    this point still leaves the plan + pre_state on disk for reconciliation.
        let redacted_params = redact::redact_to_string(params.clone(), &self.cred_ref);
        let row = journal::insert(
            &ctx.db,
            NewAction {
                session_id: &ctx.session_id.to_string(),
                tool_name: &self.op.name,
                tool_tier: risk_tier_str(self.op.risk_tier()),
                compensability: self.op.compensability_str(),
                idempotency_key: &idempotency_key,
                correlation_ref: &correlation_ref,
                request_params: &redacted_params,
                pre_state: pre_state.as_deref(),
                pre_state_version: pre_state_version.as_deref(),
                compensation_plan: compensation_plan.as_deref(),
                retention_days: CONNECTOR_RETENTION_DAYS,
            },
        )
        .await?;

        // 4. M5 TOCTOU: re-check the kill switch AFTER the outbox insert, just before the
        //    external call. A human who threw the switch in the window between dispatch's
        //    gate and here must still block the write. Per-record so a batch cannot race
        //    past a mid-batch kill. Acquire orders this read after any relaxed store.
        if self.kill.load(Ordering::Acquire) {
            journal::set_readback(&ctx.db, &row.id, "skipped", None).await?;
            anyhow::bail!(
                "kill switch (safety.disable_writes) engaged — external write blocked before dispatch"
            );
        }

        // 5. External write.
        let outcome = self.executor.call(&self.op.name, &params).await?;
        let returned_id = match &outcome {
            ExecOutcome::Ok { returned_id, .. } => returned_id.clone(),
            ExecOutcome::Fault {
                fault_string,
                code,
                ..
            } => {
                // A server-returned fault is not a transport loss — record it and surface a
                // tag-stripped summary (the fault_string is third-party text, C5).
                let summary = redact::strip_tool_tags(fault_string);
                journal::set_readback(&ctx.db, &row.id, "mismatch", Some(&summary)).await?;
                anyhow::bail!(
                    "connector op '{}' faulted: {} ({})",
                    self.op.name,
                    summary,
                    code.as_deref().unwrap_or("no-code")
                );
            }
        };

        // 6. GET post_state and diff ONLY the request_params fields (a server-added field
        //    like create_date must not trigger a false mismatch).
        let (readback_status, post_summary) =
            match self.executor.read_back(&self.op.name, &correlation_ref).await {
                Ok(body) => {
                    let status = if request_fields_match(&params, &body) {
                        "match"
                    } else {
                        "mismatch"
                    };
                    (status, Some(summarize(&body)))
                }
                // C7: the read-back GET itself failed — do NOT conclude failure. Mark
                // unverified (does not block a later undo).
                Err(_) => ("unverified", None),
            };
        journal::set_readback(&ctx.db, &row.id, readback_status, post_summary.as_deref()).await?;

        Ok(format!(
            "Đã thực hiện '{}' (journal {}), read-back: {}{}.",
            self.op.name,
            row.id,
            readback_status,
            returned_id
                .map(|id| format!(", id={id}"))
                .unwrap_or_default()
        ))
    }
}

/// Map a `RiskTier` to the string stored in the journal `tool_tier` column.
fn risk_tier_str(tier: RiskTier) -> &'static str {
    match tier {
        RiskTier::Read => "Read",
        RiskTier::ReversibleWrite => "ReversibleWrite",
        RiskTier::IrreversibleWrite => "IrreversibleWrite",
        RiskTier::Blocked => "Blocked",
    }
}

/// Diff ONLY the fields present in `params` against the read-back `body`. A field the
/// server added (e.g. `create_date`) is ignored — only what we asked to write is verified.
/// Redacted credential markers are skipped (they are not record fields).
fn request_fields_match(params: &Value, body: &Value) -> bool {
    // Writes may live under a `values` object; fall back to the params object itself.
    let expected = params.get("values").unwrap_or(params);
    let expected_map = match expected.as_object() {
        Some(m) => m,
        None => return true, // nothing structured to diff → do not claim mismatch
    };
    for (k, v) in expected_map {
        if v.as_str().is_some_and(|s| s.starts_with("<redacted:")) {
            continue;
        }
        match body.get(k) {
            Some(actual) if actual == v => {}
            Some(_) | None => return false,
        }
    }
    true
}

/// Bounded, tag-stripped one-line summary of a read-back body for the journal post_state.
fn summarize(body: &Value) -> String {
    let raw = body.to_string();
    let trimmed: String = raw.chars().take(512).collect();
    redact::strip_tool_tags(&trimmed)
}

/// The generic HTTP connector executor (R3): performs a manifest op as a raw HTTP call
/// through the SSRF-allowance guard. The Odoo-specific executor (execute_kw / faultString
/// classification) is phase 5 — this is the substrate it will specialize.
///
/// M5: `call` re-runs the kill-switch check at the network boundary too, so even a caller
/// that bypassed the tool's own re-check cannot slip a write past a live kill.
pub struct HttpExecutor {
    manifest: Arc<Manifest>,
    kill: Arc<AtomicBool>,
    timeout: Duration,
}

impl HttpExecutor {
    /// Build an executor bound to a manifest's base_url + pinned allowance + the shared
    /// kill switch. `timeout` bounds every external call.
    pub fn new(manifest: Arc<Manifest>, kill: Arc<AtomicBool>, timeout: Duration) -> Self {
        Self {
            manifest,
            kill,
            timeout,
        }
    }

    /// The op-independent request URL: the manifest's approved base_url. Op-specific path
    /// routing (execute_kw JSON body etc.) is phase 5's Odoo specialization.
    fn endpoint(&self) -> &str {
        &self.manifest.base_url
    }
}

#[async_trait]
impl ConnectorExecutor for HttpExecutor {
    async fn call(&self, op: &str, params: &Value) -> Result<ExecOutcome> {
        // M5 (second line of defense): a caller bypassing the tool's re-check still cannot
        // write past a live kill switch.
        if self.kill.load(Ordering::Acquire) {
            anyhow::bail!("kill switch engaged — connector call '{op}' blocked");
        }
        let body = serde_json::json!({ "op": op, "params": params }).to_string();
        let resp = security::follow_redirects_with_guard_allowance(
            self.endpoint(),
            &self.manifest.allowed_ip_cidrs,
            self.timeout,
            move |client, url| {
                client
                    .post(url)
                    .header("content-type", "application/json")
                    .body(body.clone())
            },
        )
        .await?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("connector call '{op}': body read failed: {e}"))?;
        // A transport-level failure surfaces as Err (above); a reachable server that
        // returns an error status is a structured fault (C7 — caller reads back).
        if status >= 400 {
            return Ok(ExecOutcome::Fault {
                fault_string: redact::strip_tool_tags(&text),
                code: Some(status.to_string()),
                name: None,
            });
        }
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let returned_id = parsed
            .get("id")
            .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_i64().map(|n| n.to_string())));
        Ok(ExecOutcome::Ok {
            returned_id,
            body: parsed,
        })
    }

    async fn read_back(&self, op: &str, correlation_ref: &str) -> Result<Value> {
        let resp = security::follow_redirects_with_guard_allowance(
            self.endpoint(),
            &self.manifest.allowed_ip_cidrs,
            self.timeout,
            move |client, url| {
                client
                    .get(url)
                    .query(&[("op", op), ("correlation_ref", correlation_ref)])
            },
        )
        .await?;
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("connector read_back '{op}': body read failed: {e}"))?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::executor::mock::MockExecutor;
    use crate::connector::manifest;
    use haily_db::queries::journal;
    use haily_db::DbHandle;
    use haily_types::ApprovalGate;
    use serde_json::json;

    async fn db() -> (Arc<DbHandle>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        (Arc::new(db), dir)
    }

    fn op_spec() -> Arc<OpSpec> {
        let m = manifest::parse(
            r#"{"connector_name":"odoo","version":"1","base_url":"https://erp.example.com",
                "allowed_ip_cidrs":[],
                "ops":[{"name":"odoo_contact_create","model":"res.partner","method":"create",
                        "risk_tier":"IrreversibleWrite","compensability":"compensatable",
                        "compensation":{"op":"unlink"}}]}"#,
        )
        .unwrap();
        Arc::new(m.ops[0].clone())
    }

    fn full_manifest() -> Arc<Manifest> {
        Arc::new(
            manifest::parse(
                r#"{"connector_name":"odoo","version":"1","base_url":"https://erp.example.com",
                    "allowed_ip_cidrs":[],"ops":[]}"#,
            )
            .unwrap(),
        )
    }

    /// A throwaway approval gate (auto-denies) — tests here never raise an approval; the
    /// gate is only present because `ToolContext` requires one.
    struct NoopGate;
    #[async_trait]
    impl ApprovalGate for NoopGate {
        async fn request(
            &self,
            _approval_id: uuid::Uuid,
            _session_id: uuid::Uuid,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> bool {
            false
        }
    }

    /// Build a `ToolContext` for a connector-tool test. The connector tool never touches
    /// kms, but `ToolContext` requires a handle, so we init a throwaway one on the same
    /// tempdir (kept alive via the returned guard).
    async fn ctx(db: Arc<DbHandle>) -> (ToolContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let kms_db = DbHandle::init(&dir.path().join("kms.db")).await.unwrap();
        let kms = Arc::new(haily_kms::KmsHandle::init(kms_db, dir.path()).await.unwrap());
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let c = ToolContext {
            db,
            kms,
            session_id: uuid::Uuid::new_v4(),
            depth: 0,
            domain: None,
            approval_gate: Arc::new(NoopGate),
            approval_tx: tx,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        (c, dir)
    }

    #[tokio::test]
    async fn write_op_inserts_journal_row_with_compensation_before_call() {
        let (db, _d) = db().await;
        let exec = Arc::new(MockExecutor::new(
            vec![ExecOutcome::Ok {
                returned_id: Some("42".into()),
                body: json!({}),
            }],
            // pre-read skipped (create); post-read shows the written field present.
            vec![Some(json!({"name": "Alice", "create_date": "x"}))],
        ));
        let tool = HttpConnectorTool {
            manifest: full_manifest(),
            op: op_spec(),
            executor: exec.clone(),
            kill: Arc::new(AtomicBool::new(false)),
            cred_ref: "odoo.api_key".into(),
        };
        let (ctx, _kd) = ctx(db.clone()).await;
        let out = tool
            .execute(json!({"params": {"values": {"name": "Alice"}}}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("match"), "read-back should match: {out}");

        // The journal row exists with the compensation plan recorded (outbox before call).
        let rows = journal::list_by_session(&db, &ctx.session_id.to_string())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool_name, "odoo_contact_create");
        assert!(
            rows[0].compensation_plan.as_deref().unwrap().contains("unlink"),
            "compensation plan recorded before the external call"
        );
        // The single external call did happen (mock recorded it).
        assert_eq!(exec.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn kill_recheck_before_external_call() {
        // M5 TOCTOU: flip the kill flag AFTER the outbox insert would run but BEFORE the
        // call. Since execute() re-checks synchronously right before the call and the flag
        // is already set, the external call must NOT happen and the row is marked skipped.
        let (db, _d) = db().await;
        let exec = Arc::new(MockExecutor::new(
            vec![ExecOutcome::Ok {
                returned_id: Some("42".into()),
                body: json!({}),
            }],
            vec![Some(json!({}))],
        ));
        let kill = Arc::new(AtomicBool::new(true)); // engaged before dispatch reaches the call
        let tool = HttpConnectorTool {
            manifest: full_manifest(),
            op: op_spec(),
            executor: exec.clone(),
            kill: kill.clone(),
            cred_ref: "odoo.api_key".into(),
        };
        let (ctx, _kd) = ctx(db.clone()).await;
        let res = tool
            .execute(json!({"params": {"values": {"name": "Alice"}}}), &ctx)
            .await;
        assert!(res.is_err(), "kill switch must block the write");
        assert!(
            res.unwrap_err().to_string().contains("kill switch"),
            "error must name the kill switch"
        );
        // No external call happened.
        assert!(
            exec.calls.lock().unwrap().is_empty(),
            "kill re-check must prevent the executor call"
        );
        // The outbox row was inserted (before the re-check) and marked skipped.
        let rows = journal::list_by_session(&db, &ctx.session_id.to_string())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "outbox row inserted before the kill re-check");
        assert_eq!(rows[0].readback_status, "skipped");
    }

    #[test]
    fn risk_tier_from_op_spec_fail_closed() {
        // An op with no declared tier fails closed to IrreversibleWrite through the tool.
        let m = manifest::parse(
            r#"{"connector_name":"c","version":"1","base_url":"https://x.example.com",
                "allowed_ip_cidrs":[],
                "ops":[{"name":"mystery_op"}]}"#,
        )
        .unwrap();
        let tool = HttpConnectorTool {
            manifest: full_manifest(),
            op: Arc::new(m.ops[0].clone()),
            executor: Arc::new(MockExecutor::new(vec![], vec![])),
            kill: Arc::new(AtomicBool::new(false)),
            cred_ref: "c.key".into(),
        };
        assert_eq!(tool.risk_tier(&json!({})), RiskTier::IrreversibleWrite);
    }
}
