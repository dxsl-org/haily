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
//! Odoo's `execute_kw`/faultString shape (phase 5, proved against a live sandbox) is now
//! reproduced by this GENERIC HTTP executor interpreting the manifest's `protocol` section
//! (Phase 4a) rather than a bespoke Odoo executor — see [`HttpExecutor::call_protocol`].
use crate::connector::credential::CredentialGetter;
use crate::connector::manifest::{Manifest, OpSpec, ProtocolSpec, ResolvedAuthScheme};
use crate::connector::odoo_fault;
use crate::connector::protocol::{self, ConnectionOverlay};
use crate::connector::{readback_diff, redact, ConnectorExecutor, ExecOutcome};
use crate::security;
use crate::{RiskTier, Tool, ToolContext};
use anyhow::{Context, Result};
use async_trait::async_trait;
use haily_db::queries::journal::{self, NewAction};
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

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
    /// M2 (Activate-and-Measure phase 4b): this manifest's content hash (`ConnectorManifestRow::content_hash`),
    /// pinned into every journal row this tool writes so undo/reconcile can detect the
    /// manifest changing/moving since the write (see `journal_undo::ConnectorResolver`).
    pub manifest_hash: String,
}

impl HttpConnectorTool {
    /// Build the outbox `compensation_plan` from the manifest's declared `compensation`
    /// template, enriched with the concrete undo TARGET so the recorded plan is executable —
    /// the whole point of the outbox is that the undo path is complete on disk before the
    /// write.
    ///
    /// Target-id resolution (fail-closed at undo time if still absent, see `journal_undo`):
    /// - **create**: the id is not known until the call RETURNS, so it is `None` here and
    ///   written back post-call via [`journal::update_compensation_plan`]. Passing a
    ///   `returned_id` fills it in.
    /// - **update/archive** (a write to an existing record): the target ids are in the request
    ///   `params` (`ids`/`id`) and are copied into the plan up front — a non-create's
    ///   compensation targets the SAME records the write touched.
    ///
    /// For an update whose compensation template carries no `values` (a plain write-back), the
    /// PREVIOUS field values are lifted from the captured `pre_state` for exactly the fields
    /// this write changes — that is what makes "restore the previous values" a real undo rather
    /// than a no-op. Fields absent from pre_state are left out (nothing to restore).
    ///
    /// `secret` (Phase 6 write-side fix): `pre_state` here is the RAW `pre_state_body`, NOT the
    /// already-scrubbed `pre_state` string stored separately — a value lifted verbatim out of it
    /// could still carry a server-reflected resolved credential. `restore_values` scrubs every
    /// lifted value through the same M3 sanitize pass BEFORE it is written into this (mutable,
    /// undo-critical) `compensation_plan` column, so the honored contract documented on
    /// `journal.rs`'s `ActionJournalRow::request_params` ("caller-sanitized") now actually holds
    /// for `compensation_plan` too.
    fn compensation_plan(
        &self,
        params: &Value,
        pre_state: Option<&Value>,
        returned_id: Option<&str>,
        secret: Option<&str>,
    ) -> Option<String> {
        let mut plan = self.op.compensation.clone()?;
        let obj = plan.as_object_mut()?;

        // Target id: returned id for a create, else the request's ids/id. A create's returned
        // id is coerced to an integer where it parses as one — Odoo record ids are integers and
        // the executor's `write`/`unlink` args builder wraps the value in a `[id]` list, so a
        // stringly `"42"` id would build `["42"]` and Odoo would reject the write.
        if let Some(id) = returned_id {
            let id_val = id
                .parse::<i64>()
                .map(Value::from)
                .unwrap_or_else(|_| Value::String(id.to_string()));
            obj.entry("id").or_insert(id_val);
        } else if !obj.contains_key("ids") && !obj.contains_key("id") {
            if let Some(ids) = params.get("ids").cloned() {
                obj.insert("ids".to_string(), ids);
            } else if let Some(id) = params.get("id").cloned() {
                obj.insert("id".to_string(), id);
            }
        }

        // A write-back compensation with no template values restores the PREVIOUS values of
        // the fields this op writes, read out of the captured pre_state.
        let is_write_back =
            obj.get("op").and_then(Value::as_str) == Some("write") && !obj.contains_key("values");
        if is_write_back {
            if let Some(restored) = restore_values(params, pre_state, secret) {
                obj.insert("values".to_string(), restored);
            }
        }
        Some(plan.to_string())
    }
}

/// The concrete record id a non-create op targets, taken from the request params: the first
/// element of `ids`, or a scalar `id`. Returned as a string for the `id_hint` read-back
/// locator (an update/archive has no client correlation ref — it is found by id).
fn params_target_id(params: &Value) -> Option<String> {
    if let Some(first) = params.get("ids").and_then(Value::as_array).and_then(|a| a.first()) {
        return first
            .as_i64()
            .map(|n| n.to_string())
            .or_else(|| first.as_str().map(str::to_string));
    }
    params
        .get("id")
        .and_then(|v| v.as_i64().map(|n| n.to_string()).or_else(|| v.as_str().map(str::to_string)))
}

/// For each field the request writes (under `values`), look up its PREVIOUS value in
/// `pre_state` — the object to write back to undo the change. Returns `None` when there is
/// nothing to restore (no pre_state or no writable fields), so the caller leaves the
/// compensation template untouched rather than writing an empty `values`.
///
/// `pre_state` is the RAW pre-write body (unlike the already-scrubbed `pre_state` string the
/// journal row separately stores) — every lifted value is run through
/// [`sanitize_restored_value`] before insertion so a resolved credential a server reflected
/// back into a to-be-restored field can never reach `compensation_plan` (Phase 6 write-side fix).
fn restore_values(params: &Value, pre_state: Option<&Value>, secret: Option<&str>) -> Option<Value> {
    let pre = pre_state?;
    let written = params.get("values")?.as_object()?;
    let mut restored = serde_json::Map::new();
    for key in written.keys() {
        if let Some(prev) = pre.get(key) {
            restored.insert(key.clone(), sanitize_restored_value(prev, secret));
        }
    }
    if restored.is_empty() {
        None
    } else {
        Some(Value::Object(restored))
    }
}

/// Recursively scrub every string leaf of `value` through [`redact::sanitize_third_party_body`]
/// (M3 secret-scrub + C5 tag-strip) — applied to values lifted verbatim out of the RAW
/// `pre_state_body` before they enter the mutable, undo-critical `compensation_plan` column.
/// Non-string leaves (numbers/bools/null) pass through unchanged: nothing to scrub, and
/// stringifying them would corrupt the value's type for a later compensating write.
fn sanitize_restored_value(value: &Value, secret: Option<&str>) -> Value {
    match value {
        Value::String(s) => Value::String(redact::sanitize_third_party_body(s, secret)),
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|v| sanitize_restored_value(v, secret)).collect())
        }
        Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), sanitize_restored_value(v, secret)))
                .collect(),
        ),
        other => other.clone(),
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
        let mut params = args.get("params").cloned().unwrap_or(args.clone());
        // Honor a caller-supplied correlation_ref so the ref the executor WRITES into the record
        // and the ref this tool READS BACK by are the same value. Without this the executor
        // would write the params' ref while read-back searched a freshly generated uuid — the
        // create's post-read would find nothing and mark a false `mismatch`. Absent → generate
        // one and inject it into params so the executor embeds it too (C7 discoverability).
        let correlation_ref = params
            .get("correlation_ref")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                let corr = uuid::Uuid::new_v4().to_string();
                if let Some(obj) = params.as_object_mut() {
                    obj.insert("correlation_ref".to_string(), Value::String(corr.clone()));
                }
                corr
            });
        let idempotency_key = format!("{}:{}", self.op.name, correlation_ref);
        // The concrete target id for a non-create (update/archive) — its pre_state read has no
        // client ref, so read-back must locate the record by id. Taken from the request params.
        let target_id = params_target_id(&params);

        // 1. GET pre_state (+version token). Skipped for creates — there is no record yet.
        //    The parsed body is kept so an update's compensation can restore previous values.
        let (pre_state_body, pre_state, pre_state_version) = if self.op.is_create() {
            (None, None, None)
        } else {
            match self
                .executor
                .read_back(&self.op.name, &correlation_ref, None, target_id.as_deref())
                .await
            {
                Ok(body) => {
                    let version = body
                        .get("write_date")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    // M3 + C5: scrub the resolved secret VALUE (if any) alongside the
                    // tool-protocol tag-strip before this third-party body is persisted —
                    // tag-stripping alone would not remove a credential a server reflects
                    // back in a pre-write read.
                    let secret = self.executor.resolved_secret().await;
                    let safe = redact::sanitize_third_party_body(&body.to_string(), secret.as_deref());
                    (Some(body.clone()), Some(safe), version)
                }
                // A failed pre-read is not fatal — record an empty pre_state and proceed;
                // the post-write read-back is the runtime backstop.
                Err(_) => (None, None, None),
            }
        };

        // 2. Build the compensation plan: an UPDATE/ARCHIVE carries its target ids (from the
        //    request) + restored previous values now; a CREATE's returned id is filled in
        //    post-call (step 5b). `comp_secret` is resolved fresh here (never cached, mirroring
        //    every other `resolved_secret()` call site in this method) so `restore_values` can
        //    scrub it out of any RAW pre_state field it lifts into `values` (Phase 6 fix).
        let comp_secret = self.executor.resolved_secret().await;
        let compensation_plan =
            self.compensation_plan(&params, pre_state_body.as_ref(), None, comp_secret.as_deref());

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
                turn_id: Some(&ctx.turn_id.to_string()),
                retention_days: CONNECTOR_RETENTION_DAYS,
                manifest_hash: Some(&self.manifest_hash),
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
                // A server-returned fault is not a transport loss — record it. M3 + C5: the
                // fault_string is third-party text that may already be tag-stripped by the
                // executor, but a reflected credential survives a tag-strip — scrub the
                // resolved secret VALUE too before this summary reaches the journal.
                let secret = self.executor.resolved_secret().await;
                let summary = redact::sanitize_third_party_body(fault_string, secret.as_deref());
                journal::set_readback(&ctx.db, &row.id, "mismatch", Some(&summary)).await?;
                anyhow::bail!(
                    "connector op '{}' faulted: {} ({})",
                    self.op.name,
                    summary,
                    code.as_deref().unwrap_or("no-code")
                );
            }
        };

        // 5b. Write the returned id back into the compensation plan for a create. The plan is
        //     journaled at step 2 with NO id (a create has no record id yet); its archive/write
        //     compensation targets that id, so without this write-back the undo would run
        //     `write(null, {active:false})` — targeting no record (or, worse, every record).
        //     `compensation_plan` is a mutable processing column (not append-only guarded), so
        //     rewriting it here is permitted. `journal_undo` fail-closes if the id is still
        //     absent at undo time (a lost create leaves no compensation target).
        if self.op.is_create() {
            if let Some(id) = returned_id.as_deref() {
                // `pre_state` is `None` for a create (step 1), so `restore_values` never runs
                // and there is nothing for `secret` to scrub here — `None` is correct, not a
                // shortcut.
                if let Some(plan) = self.compensation_plan(&params, None, Some(id), None) {
                    journal::update_compensation_plan(&ctx.db, &row.id, &plan).await?;
                }
            }
        }

        // 6. GET post_state and diff ONLY the request_params fields (a server-added field
        //    like create_date must not trigger a false mismatch). The record is located by the
        //    correlation_ref (when the model has a correlation field) OR by its id: a create's
        //    RETURNED id, else the update/archive target id. A create whose model has no
        //    correlation field (mail.activity) is thus still verifiable by its returned id;
        //    with neither locator the read-back is empty → `unverified` (fail-closed, does not
        //    block a later undo).
        let post_id = returned_id.as_deref().or(target_id.as_deref());
        let (readback_status, post_summary, post_version) = match self
            .executor
            .read_back(&self.op.name, &correlation_ref, None, post_id)
            .await
        {
            Ok(body) => {
                let status = if request_fields_match(&params, &body) {
                    "match"
                } else {
                    "mismatch"
                };
                // Capture the post-write write_date as the C10 self-undo baseline ONLY for a
                // non-create update/archive: the undo then refuses on a THIRD-PARTY change
                // BEYOND our own write, not on the change our forward write itself made (which
                // always bumps write_date). A CREATE is deliberately NOT versioned — its undo
                // (archive/unlink) is guarded by read-back-before-comp (already-gone =
                // already-done) + the target-id guard, and a post-create write_date would
                // wrongly refuse the undo once the record is legitimately unlinked/archived.
                let version = if self.op.is_create() {
                    None
                } else {
                    body.get("write_date")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                };
                // M3: scrub the resolved secret VALUE (if any) alongside the C5 tag-strip
                // `summarize` already does — a reflected credential in the post-write body
                // must not reach the journal any more than in the fault/pre_state paths.
                let secret = self.executor.resolved_secret().await;
                (status, Some(summarize(&body, secret.as_deref())), version)
            }
            // C7: the read-back GET itself failed — do NOT conclude failure. Mark
            // unverified (does not block a later undo).
            Err(_) => ("unverified", None, None),
        };
        journal::set_readback(&ctx.db, &row.id, readback_status, post_summary.as_deref()).await?;
        if let Some(version) = post_version.as_deref() {
            journal::set_post_state_version(&ctx.db, &row.id, version).await?;
        }

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
/// Delegates to [`readback_diff`] so the comparison normalizes Odoo's read representation
/// (many2one `[id, name]` → id, `false` ↔ unset, id-set relations, int/float) — a format-only
/// difference is not a false mismatch, while a genuine scalar value difference still is.
fn request_fields_match(params: &Value, body: &Value) -> bool {
    // Writes may live under a `values` object; fall back to the params object itself.
    let expected = params.get("values").unwrap_or(params);
    readback_diff::request_fields_match(expected, body)
}

/// Bounded, secret-scrubbed (M3) + tag-stripped (C5) one-line summary of a read-back body for
/// the journal post_state. `secret` is the executor's currently-resolved credential (if any,
/// see [`ConnectorExecutor::resolved_secret`]) — scrubbed from the body BEFORE truncation so a
/// secret cannot be half-truncated into an unrecognizable-but-still-leaked fragment.
fn summarize(body: &Value, secret: Option<&str>) -> String {
    let raw = redact::redact_secret_value(&body.to_string(), secret);
    let trimmed: String = raw.chars().take(512).collect();
    redact::strip_tool_tags(&trimmed)
}

/// The generic HTTP connector executor (R3/phase-3): performs a manifest op either as the
/// v1 generic `{"op","params"}` POST (no `protocol` declared — unchanged behavior) or, when
/// `manifest.protocol` is present, by INTERPRETING it (envelope substitution, per-method arg
/// shaping, fault classification, read-back) — the step that lets a REST/JSON-RPC connector
/// be expressed entirely as manifest data, built to reproduce `OdooExecutor`'s `execute_kw`
/// shape exactly (M5b parity).
///
/// M5: `call` re-runs the kill-switch check at the network boundary too, so even a caller
/// that bypassed the tool's own re-check cannot slip a write past a live kill.
///
/// Auth (phase 2, C1): a manifest's `auth` section is applied per-request, ONLY when the
/// request's target host equals the (overlay-resolved, M4) base_url host. `follow_redirects_
/// with_guard_allowance*` re-invokes the request-builder closure per redirect hop with that
/// hop's own URL, and its SSRF re-vet only blocks private/metadata IPs — a `302` to a
/// DIFFERENT public host would otherwise still receive the credential. The host check is the
/// actual exfil control here, not the SSRF guard. The SAME host gate governs the phase-3
/// envelope `{{key}}` token (C1 carryover): a cross-host hop resends the request body with
/// `{{key}}` resolved to an EMPTY string, never the real secret.
pub struct HttpExecutor {
    manifest: Arc<Manifest>,
    kill: Arc<AtomicBool>,
    timeout: Duration,
    /// See [`CredentialGetter`]. `None` preserves pre-phase-2 behavior: a manifest with no
    /// `auth` section sends no credential regardless; a manifest WITH an `auth` section but
    /// no injected getter fails closed (never an unauthenticated request for a manifest that
    /// declares auth) — see [`HttpExecutor::resolve_auth`].
    credential_getter: Option<Arc<dyn CredentialGetter>>,
    /// The M4 per-deployment overlay (base_url override, `db`/`uid` envelope tokens, cred_ref
    /// override) — see [`ConnectionOverlay`]. `None` preserves pre-phase-3 behavior (no
    /// override; `{{db}}`/`{{uid}}` in an envelope then fail closed as unresolvable tokens).
    connection: Option<ConnectionOverlay>,
    /// TEST ONLY — never true in production. Mirrors `OdooExecutor::allow_loopback`: lets the
    /// SSRF allowance permit a pinned LOOPBACK address so this executor's own test suite can
    /// exercise the real redirect/auth path against a local fixture server. Production wiring
    /// (`Orchestrator::init` → `register_connectors`) always constructs via
    /// [`HttpExecutorConfig::production`], which sets this `false`.
    allow_loopback: bool,
}

/// Constructor parameters for [`HttpExecutor`], grouped so adding a field (credential
/// getter, then the test-only loopback flag, then the M4 connection overlay) never grows the
/// constructor's arity. Mirrors `OdooExecutorConfig`'s `production()` + builder shape.
pub struct HttpExecutorConfig {
    pub manifest: Arc<Manifest>,
    pub kill: Arc<AtomicBool>,
    pub timeout: Duration,
    /// See [`HttpExecutor::credential_getter`]. Defaults to `None` via
    /// [`HttpExecutorConfig::production`]; opt in via [`Self::with_credential_getter`].
    pub credential_getter: Option<Arc<dyn CredentialGetter>>,
    /// See [`HttpExecutor::connection`]. Defaults to `None` via
    /// [`HttpExecutorConfig::production`]; opt in via [`Self::with_connection_overlay`].
    pub connection: Option<ConnectionOverlay>,
    /// TEST ONLY — see [`HttpExecutor::allow_loopback`]. Defaults `false` via
    /// [`HttpExecutorConfig::production`]; only this crate's own test suite sets it `true`.
    pub allow_loopback: bool,
}

impl HttpExecutorConfig {
    /// Build a production config: no keyring credential source, no connection overlay,
    /// loopback SSRF carve-out disabled. Use this at every non-test construction site so the
    /// TEST-ONLY `allow_loopback` can never be set by accident.
    #[must_use]
    pub fn production(manifest: Arc<Manifest>, kill: Arc<AtomicBool>, timeout: Duration) -> Self {
        Self {
            manifest,
            kill,
            timeout,
            credential_getter: None,
            connection: None,
            allow_loopback: false,
        }
    }

    /// Opt into (or explicitly omit) an injected credential source. Takes an `Option` rather
    /// than a bare `Arc` because callers (`register_connectors`) already hold an
    /// `Option<Arc<dyn CredentialGetter>>` — a manifest declaring no `auth` never reads it,
    /// so "no getter configured at all" is a legitimate, common case, not an error.
    #[must_use]
    pub fn with_credential_getter(mut self, getter: Option<Arc<dyn CredentialGetter>>) -> Self {
        self.credential_getter = getter;
        self
    }

    /// Opt into (or explicitly omit) the M4 per-deployment connection overlay.
    #[must_use]
    pub fn with_connection_overlay(mut self, overlay: Option<ConnectionOverlay>) -> Self {
        self.connection = overlay;
        self
    }
}

impl HttpExecutor {
    /// Build an executor from its config. The credential is NOT read here — only at call
    /// time (C4), so a rotated key is picked up without reconstructing the executor.
    #[must_use]
    pub fn new(cfg: HttpExecutorConfig) -> Self {
        Self {
            manifest: cfg.manifest,
            kill: cfg.kill,
            timeout: cfg.timeout,
            credential_getter: cfg.credential_getter,
            connection: cfg.connection,
            allow_loopback: cfg.allow_loopback,
        }
    }

    /// The op-independent request URL: the M4 overlay's `base_url_override` when set, else
    /// the manifest's own approved `base_url`.
    fn endpoint(&self) -> &str {
        match &self.connection {
            Some(overlay) => overlay.effective_base_url(&self.manifest.base_url),
            None => &self.manifest.base_url,
        }
    }

    /// The endpoint under `protocol.endpoint_suffix` (e.g. `/jsonrpc`), or [`Self::endpoint`]
    /// unchanged when the protocol declares no suffix. `proto` names the parameter (rather
    /// than `protocol`) purely to avoid shadowing the `protocol` MODULE this file imports —
    /// both would compile (separate namespaces), but a distinct name reads less ambiguously.
    fn protocol_endpoint(&self, proto: &ProtocolSpec) -> String {
        match proto.endpoint_suffix.as_deref() {
            Some(suffix) => format!("{}{suffix}", self.endpoint().trim_end_matches('/')),
            None => self.endpoint().to_string(),
        }
    }

    /// Resolve the manifest's declared auth against the injected [`CredentialGetter`], once
    /// per `call`/`read_back` invocation — never cached on `self` (mirrors
    /// `OdooExecutor::read_key`), so a rotated secret is picked up without reconstructing the
    /// executor. `Ok(None)` means the manifest declares no `auth` at all (v1 backward
    /// compatibility: no header/param/envelope-key is ever applied). An `auth`-declaring
    /// manifest with an unresolvable secret is a hard `Err` (fail-closed, C1/m2's headline
    /// contract) — the caller must never fall back to sending the request unauthenticated.
    /// The credential reference NAME is the M4 overlay's `cred_ref_override` when set, else
    /// the manifest auth's own `cred_ref`.
    async fn resolve_auth(&self) -> Result<Option<(ResolvedAuthScheme, String)>> {
        let Some(auth) = &self.manifest.auth else {
            return Ok(None);
        };
        // `manifest::parse` already validated the scheme at load time (an unresolvable
        // scheme never registers a tool at all); resolving again here is cheap and keeps the
        // resolved shape colocated with its secret rather than caching it separately.
        let scheme = auth
            .resolve()
            .map_err(|e| anyhow::anyhow!("connector '{}': {e}", self.manifest.connector_name))?;
        let cred_ref: &str = self
            .connection
            .as_ref()
            .map_or(auth.cred_ref.as_str(), |o| o.effective_cred_ref(&auth.cred_ref));
        let getter = self.credential_getter.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "connector '{}' declares auth (cred_ref '{cred_ref}') but no credential getter \
                 is configured — refusing to send an unauthenticated request",
                self.manifest.connector_name,
            )
        })?;
        let secret = getter
            .get_secret(cred_ref)
            .await
            .with_context(|| format!("credential getter failed for '{cred_ref}'"))?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "credential '{cred_ref}' not available — refusing to send an \
                     unauthenticated request for a manifest that declares auth",
                )
            })?;
        Ok(Some((scheme, secret)))
    }

    /// The EFFECTIVE endpoint's normalized host (lowercase, no trailing dot; overlay-resolved,
    /// M4) — the C1 anchor every redirect hop's host is compared against. Parsed ONCE per
    /// call so the per-hop closure (a plain `Fn`, which cannot itself return a `Result`) only
    /// ever does infallible string comparison.
    fn base_host(&self) -> Result<String> {
        let endpoint = self.endpoint();
        let url = Url::parse(endpoint)
            .map_err(|e| anyhow::anyhow!("connector base_url '{endpoint}' invalid: {e}"))?;
        url.host_str()
            .map(normalize_host)
            .ok_or_else(|| anyhow::anyhow!("connector base_url '{endpoint}' has no host"))
    }

    /// POST `body_with_secret` on a hop whose host matches the manifest's own (C1), or
    /// `body_without_secret` on any other hop — extending the SAME per-hop host gate that
    /// already governs header/query-param auth to a secret embedded IN the body (the
    /// protocol-path `{{key}}` envelope token, Odoo-shaped). A caller with no body-embedded
    /// secret passes the SAME string for both parameters, making the gate a no-op. Header/
    /// query-param auth (when the manifest ALSO declares one) is applied identically on top,
    /// gated by the same host check.
    async fn send_post(&self, endpoint: &str, body_with_secret: String, body_without_secret: String) -> Result<(u16, String)> {
        let auth = self.resolve_auth().await?;
        let base_host = self.base_host()?;
        let resp = security::follow_redirects_with_guard_allowance_loopback(
            endpoint,
            &self.manifest.allowed_ip_cidrs,
            self.timeout,
            self.allow_loopback,
            move |client, url| {
                let is_home = hop_host_matches(url, &base_host);
                let body = if is_home { body_with_secret.clone() } else { body_without_secret.clone() };
                let target = hop_target_url(url, &auth, &base_host);
                let mut builder = client.post(&target).header("content-type", "application/json").body(body);
                if let Some((scheme, secret)) = &auth {
                    if is_home {
                        builder = apply_auth(builder, scheme, secret);
                    }
                }
                builder
            },
        )
        .await?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("connector call: body read failed: {e}"))?;
        Ok((status, text))
    }

    /// GET `endpoint` with `query`, applying auth (host-gated, C1) exactly as [`Self::send_post`].
    async fn send_get(&self, endpoint: &str, query: &[(&str, &str)]) -> Result<(u16, String)> {
        let auth = self.resolve_auth().await?;
        let base_host = self.base_host()?;
        let query_owned: Vec<(String, String)> =
            query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let resp = security::follow_redirects_with_guard_allowance_loopback(
            endpoint,
            &self.manifest.allowed_ip_cidrs,
            self.timeout,
            self.allow_loopback,
            move |client, url| {
                let target = hop_target_url(url, &auth, &base_host);
                let mut builder = client.get(&target).query(&query_owned);
                if let Some((scheme, secret)) = &auth {
                    if hop_host_matches(url, &base_host) {
                        builder = apply_auth(builder, scheme, secret);
                    }
                }
                builder
            },
        )
        .await?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("connector read_back: body read failed: {e}"))?;
        Ok((status, text))
    }

    /// Substitute `protocol.envelope` with `{model, method, args, kwargs}` plus, when the M4
    /// overlay supplies them, `{db, uid}`, plus `key` = `secret.unwrap_or("")`. `secret` is
    /// ALWAYS resolvable as a token (never an unresolvable-token error) — the caller decides
    /// per-hop (C1) whether to pass the real value or `None`/empty, mirroring the
    /// header/query-param auth's own "drop the credential on a cross-host hop" semantics. A
    /// `{{db}}`/`{{uid}}` referenced with no overlay value DOES fail closed (an unresolvable
    /// token) — those identify WHICH database/user, and silently sending a wrong/absent one
    /// is a correctness bug the manifest author must fix, unlike the secret's deliberate
    /// per-hop drop.
    fn build_envelope(
        &self,
        proto: &ProtocolSpec,
        model: &str,
        method: &str,
        args: Value,
        kwargs: Value,
        secret: Option<&str>,
    ) -> Result<String> {
        let envelope = proto
            .envelope
            .as_ref()
            .context("connector protocol declares no envelope template")?;
        let mut ctx = Map::new();
        ctx.insert("model".to_string(), Value::String(model.to_string()));
        ctx.insert("method".to_string(), Value::String(method.to_string()));
        ctx.insert("args".to_string(), args);
        ctx.insert("kwargs".to_string(), kwargs);
        if let Some(overlay) = &self.connection {
            if let Some(db) = &overlay.db {
                ctx.insert("db".to_string(), Value::String(db.clone()));
            }
            if let Some(uid) = overlay.uid {
                ctx.insert("uid".to_string(), Value::from(uid));
            }
        }
        ctx.insert("key".to_string(), Value::String(secret.unwrap_or("").to_string()));
        let body = protocol::substitute(envelope, &ctx)?;
        Ok(body.to_string())
    }

    /// The protocol-interpreting `call()` path: builds the wire body PURELY from
    /// `protocol` templates + substitution, mirroring `OdooExecutor::call`'s method dispatch
    /// (create embeds the correlation ref + prevalidates; write/unlink/read shape `[ids, ...]`;
    /// any other method passes the caller's own `args` through) so an Odoo-shaped manifest
    /// reproduces `OdooExecutor` exactly (M5b).
    async fn call_protocol(&self, proto: &ProtocolSpec, op: &str, params: &Value) -> Result<ExecOutcome> {
        let (model, method) = protocol::op_resolve::resolve_op_model_method(&self.manifest, op, params)?;
        let correlation_ref = params.get("correlation_ref").and_then(Value::as_str).unwrap_or("").to_string();
        let mut values = params.get("values").cloned().unwrap_or(Value::Null);

        if method == "create" {
            protocol::op_resolve::prevalidate(&proto.prevalidate, &model, &values)?;
            if let Some(field) = protocol::op_resolve::correlation_field_for(&self.manifest, &model) {
                if let Some(obj) = values.as_object_mut() {
                    obj.entry(field).or_insert_with(|| Value::String(correlation_ref.clone()));
                }
            }
        }
        let ids = params
            .get("ids")
            .cloned()
            .or_else(|| params.get("id").map(|id| serde_json::json!([id])))
            .unwrap_or(Value::Null);

        let mut shape_ctx = Map::new();
        shape_ctx.insert("values".to_string(), values);
        shape_ctx.insert("ids".to_string(), ids);
        shape_ctx.insert("correlation_ref".to_string(), Value::String(correlation_ref));
        let fallback_args = params.get("args").cloned().unwrap_or_else(|| serde_json::json!([]));
        let args = protocol::shape::shape_args(&proto.methods, &method, &shape_ctx, fallback_args)?;
        let kwargs = serde_json::json!({ "context": proto.context.clone().unwrap_or_else(|| serde_json::json!({})) });

        let secret = self.resolve_auth().await?;
        let secret_str = secret.as_ref().map(|(_, s)| s.as_str());
        let body_with = self.build_envelope(proto, &model, &method, args.clone(), kwargs.clone(), secret_str)?;
        let body_without = self.build_envelope(proto, &model, &method, args, kwargs, None)?;
        let endpoint = self.protocol_endpoint(proto);
        let (status, text) = self.send_post(&endpoint, body_with, body_without).await?;
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(self.outcome_from_parsed(proto, status, &text, parsed))
    }

    /// Classify a protocol call's response into an `ExecOutcome`, reusing
    /// `odoo_fault::extract_fault` (the SAME JSON-RPC error-shape reader `OdooExecutor` uses)
    /// so an Odoo-shaped `error` object classifies identically. A connector with no such
    /// object but an HTTP error status still classifies via a `status`-keyed `fault_rules`
    /// entry, rather than being silently read as success.
    fn outcome_from_parsed(&self, proto: &ProtocolSpec, status: u16, text: &str, body: Value) -> ExecOutcome {
        if let Some(fault) = odoo_fault::extract_fault(&body) {
            let token = protocol::fault::classify_fault(&proto.fault_rules, &fault, Some(status));
            return ExecOutcome::Fault {
                fault_string: redact::strip_tool_tags(&fault.fault_string),
                code: Some(token),
                name: fault.name,
            };
        }
        if status >= 400 {
            let fault = odoo_fault::OdooFault {
                code: Some(status.to_string()),
                name: None,
                fault_string: text.to_string(),
            };
            let token = protocol::fault::classify_fault(&proto.fault_rules, &fault, Some(status));
            return ExecOutcome::Fault {
                fault_string: redact::strip_tool_tags(&fault.fault_string),
                code: Some(token),
                name: None,
            };
        }
        let result = body.get("result").cloned().unwrap_or(Value::Null);
        let returned_id = extract_first_id(&result);
        ExecOutcome::Ok { returned_id, body: result }
    }

    /// The protocol-interpreting `read_back()` path: resolves the model (manifest op, else
    /// the compensation model hint, else fail-closed), builds the locate-domain + kwargs, and
    /// POSTs the same envelope shape `call_protocol` uses with `method="search_read"` —
    /// mirroring `OdooExecutor::read_back` exactly (M5b).
    async fn read_back_protocol(
        &self,
        proto: &ProtocolSpec,
        op: &str,
        correlation_ref: &str,
        model_hint: Option<&str>,
        id_hint: Option<&str>,
    ) -> Result<Value> {
        let model = match protocol::op_resolve::resolve_op_model_method(&self.manifest, op, &Value::Null) {
            Ok((model, _method)) => model,
            Err(_) => match model_hint {
                Some(m) => m.to_string(),
                None => self
                    .manifest
                    .ops
                    .iter()
                    .find_map(|o| o.model.clone())
                    .with_context(|| format!("read_back '{op}': no model hint and none resolvable from manifest"))?,
            },
        };
        let corr_field = protocol::op_resolve::correlation_field_for(&self.manifest, &model);
        let domain = protocol::readback::build_domain(id_hint, corr_field.as_deref(), correlation_ref);
        let kwargs = protocol::readback::build_kwargs(proto.context.as_ref(), proto.readback.as_ref());

        let secret = self.resolve_auth().await?;
        let secret_str = secret.as_ref().map(|(_, s)| s.as_str());
        let body_with = self.build_envelope(proto, &model, "search_read", domain.clone(), kwargs.clone(), secret_str)?;
        let body_without = self.build_envelope(proto, &model, "search_read", domain, kwargs, None)?;
        let endpoint = self.protocol_endpoint(proto);
        let (_status, text) = self.send_post(&endpoint, body_with, body_without).await?;
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if let Some(fault) = odoo_fault::extract_fault(&parsed) {
            let safe = redact::strip_tool_tags(&fault.fault_string);
            anyhow::bail!("connector read_back '{op}' faulted: {safe}");
        }
        let result = parsed.get("result").cloned().unwrap_or(Value::Null);
        Ok(protocol::readback::unwrap_first(result, proto.readback.as_ref()))
    }
}

/// Extract a record id from a `create`/`search`/`read`-shaped result — ported from
/// `OdooExecutor::extract_id` for parity (M5b): `create` returns the new integer id;
/// `search` returns `[ids]`; `read` returns `[{...}]`.
fn extract_first_id(result: &Value) -> Option<String> {
    match result {
        Value::Number(n) => Some(n.to_string()),
        Value::Array(arr) => arr.first().and_then(|first| match first {
            Value::Number(n) => Some(n.to_string()),
            Value::Object(o) => o.get("id").and_then(|v| v.as_i64()).map(|n| n.to_string()),
            _ => None,
        }),
        _ => None,
    }
}

/// Lowercase + strip a trailing dot, so `Example.com` and `example.com.` compare equal to
/// `example.com` — the same normalization the SSRF guard's own host handling assumes.
fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// C1: `true` only when `hop_url`'s host equals `base_host` (both normalized). Deliberately
/// compares HOST ONLY, never the full URL — a scheme/port/path difference on the SAME host
/// (e.g. a load balancer redirecting to a different port on itself) must still carry auth,
/// while a different host must never carry it. An unparseable hop URL fails closed (`false`
/// — no auth) rather than panicking or guessing.
fn hop_host_matches(hop_url: &str, base_host: &str) -> bool {
    Url::parse(hop_url)
        .ok()
        .and_then(|u| u.host_str().map(normalize_host))
        .is_some_and(|h| h == base_host)
}

/// Apply a resolved auth scheme to a per-hop request builder. `query-param` is applied via
/// the builder's own `.query()` (never by mutating the URL string the redirect follower
/// carries forward) so the secret can never leak into the next hop's `Location`-derived URL
/// or its diagnostic log (m1).
fn apply_auth(builder: reqwest::RequestBuilder, scheme: &ResolvedAuthScheme, secret: &str) -> reqwest::RequestBuilder {
    match scheme {
        ResolvedAuthScheme::Bearer => builder.bearer_auth(secret),
        ResolvedAuthScheme::Header(name) => builder.header(name.as_str(), secret),
        ResolvedAuthScheme::QueryParam(name) => builder.query(&[(name.as_str(), secret)]),
    }
}

/// The actual request-target URL for one hop. When a `query-param` auth scheme applies to
/// THIS hop (host matches, C1), any EXISTING occurrence of the auth's own param name is
/// stripped from the hop URL first — `reqwest::RequestBuilder::query` APPENDS rather than
/// replaces, so a same-host redirect whose `Location` happens to echo a stale or
/// attacker-supplied value under the SAME key would otherwise ride along next to our
/// authoritative one (`?api_key=stale&api_key=ours`), leaving it to the receiving server's
/// (unspecified) duplicate-key handling which one wins. Every other case — host mismatch, a
/// non-query-param scheme, or no auth at all — uses the hop URL unchanged.
fn hop_target_url(hop_url: &str, auth: &Option<(ResolvedAuthScheme, String)>, base_host: &str) -> String {
    match auth {
        Some((ResolvedAuthScheme::QueryParam(name), _)) if hop_host_matches(hop_url, base_host) => {
            strip_query_param(hop_url, name)
        }
        _ => hop_url.to_string(),
    }
}

/// Remove every occurrence of `param_name` from `url`'s query string, leaving other params
/// (if any) intact and in order. An unparseable `url` is returned unchanged (the caller's
/// own guard rejects it before this ever matters for a real request).
fn strip_query_param(url: &str, param_name: &str) -> String {
    let Ok(mut parsed) = Url::parse(url) else {
        return url.to_string();
    };
    let remaining: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| k != param_name)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if remaining.is_empty() {
        parsed.set_query(None);
    } else {
        let mut qp = parsed.query_pairs_mut();
        qp.clear();
        for (k, v) in &remaining {
            qp.append_pair(k, v);
        }
        drop(qp);
    }
    parsed.to_string()
}

#[async_trait]
impl ConnectorExecutor for HttpExecutor {
    async fn call(&self, op: &str, params: &Value) -> Result<ExecOutcome> {
        // M5 (second line of defense): a caller bypassing the tool's re-check still cannot
        // write past a live kill switch.
        if self.kill.load(Ordering::Acquire) {
            anyhow::bail!("kill switch engaged — connector call '{op}' blocked");
        }
        // Phase 3: a manifest declaring `protocol` is INTERPRETED (envelope substitution,
        // per-method shaping, fault classification) instead of the v1 generic body below.
        if let Some(proto) = &self.manifest.protocol {
            return self.call_protocol(proto, op, params).await;
        }
        // v1 generic path (no `protocol` declared): unchanged `{"op","params"}` POST. No
        // secret is ever embedded in this body, so the SAME string is passed for both
        // `send_post` parameters — the C1 host gate is then a no-op (nothing to drop).
        let body = serde_json::json!({ "op": op, "params": params }).to_string();
        let (status, text) = self.send_post(self.endpoint(), body.clone(), body).await?;
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

    async fn read_back(
        &self,
        op: &str,
        correlation_ref: &str,
        model_hint: Option<&str>,
        id_hint: Option<&str>,
    ) -> Result<Value> {
        if let Some(proto) = &self.manifest.protocol {
            return self.read_back_protocol(proto, op, correlation_ref, model_hint, id_hint).await;
        }
        // v1 generic path: the executor routes purely by op/correlation_ref; it has no
        // per-model path, so the model/id hints are unused here (only the protocol-
        // interpreting path resolves a model and locates by id).
        //
        // read_back is a GET, but a query-param auth scheme (and the C1 host check) still
        // applies to it — auth is not call()-only.
        let (_status, text) = self
            .send_get(self.endpoint(), &[("op", op), ("correlation_ref", correlation_ref)])
            .await?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// M3: the secret this executor would apply RIGHT NOW (host-independent — the caller
    /// scrubs it from a THIRD-PARTY body regardless of which host actually replied), so a
    /// reflected credential in a fault/pre_state/post_state body never reaches the journal.
    /// `None` when the manifest declares no `auth`, or resolution fails (nothing to scrub).
    async fn resolved_secret(&self) -> Option<String> {
        self.resolve_auth().await.ok().flatten().map(|(_, secret)| secret)
    }

    /// M6a: `None` when the manifest declares no `auth` (nothing to preflight — every other
    /// caller treats `None` like "fine, proceed"). When `auth` IS declared, `Some(true)`
    /// means [`Self::resolve_auth`] would succeed right now; `Some(false)` means it would
    /// fail (locked keyring, unconfigured getter, empty secret) — the exact condition
    /// `journal_undo`'s pending-undo path checks for BEFORE attempting a compensation call
    /// that would otherwise fail generically and burn an undo-attempt.
    async fn credential_preflight(&self) -> Option<bool> {
        self.manifest.auth.as_ref()?;
        Some(self.resolve_auth().await.is_ok())
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
            turn_id: uuid::Uuid::new_v4(),
            depth: 0,
            domain: None,
            approval_gate: Arc::new(NoopGate),
            approval_tx: tx,
            cancel: tokio_util::sync::CancellationToken::new(),
            turn_deletes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_journal_id: Arc::new(std::sync::Mutex::new(None)),
            run_id: None,
        };
        (c, dir)
    }

    /// Wraps a [`MockExecutor`] to also resolve a fixed secret (`MockExecutor` itself never
    /// overrides `resolved_secret`, per its trait default) — the write-side compensation-plan
    /// scrub test needs `execute()`'s `self.executor.resolved_secret()` call to return
    /// `Some(secret)` so the fix under test has something to scrub.
    struct SecretMockExecutor {
        inner: MockExecutor,
        secret: String,
    }

    #[async_trait]
    impl ConnectorExecutor for SecretMockExecutor {
        async fn call(&self, op: &str, params: &Value) -> Result<ExecOutcome> {
            self.inner.call(op, params).await
        }
        async fn read_back(
            &self,
            op: &str,
            correlation_ref: &str,
            model_hint: Option<&str>,
            id_hint: Option<&str>,
        ) -> Result<Value> {
            self.inner.read_back(op, correlation_ref, model_hint, id_hint).await
        }
        async fn resolved_secret(&self) -> Option<String> {
            Some(self.secret.clone())
        }
    }

    /// An update-style op whose compensation is a plain write-back (`{"op":"write"}`, no
    /// `values` template) — the `restore_values` path this test exercises only ever runs for
    /// this shape (a create's plan has no pre_state to restore from).
    fn update_op_spec() -> Arc<OpSpec> {
        let m = manifest::parse(
            r#"{"connector_name":"odoo","version":"1","base_url":"https://erp.example.com",
                "allowed_ip_cidrs":[],
                "ops":[{"name":"odoo_contact_update","model":"res.partner","method":"write",
                        "risk_tier":"ReversibleWrite","compensability":"compensatable",
                        "compensation":{"op":"write"}}]}"#,
        )
        .unwrap();
        Arc::new(m.ops[0].clone())
    }

    #[tokio::test]
    async fn compensation_plan_scrubs_resolved_secret_from_restored_pre_state_value() {
        // Phase 6 write-side fix: `restore_values` lifts a field's PREVIOUS value out of the
        // RAW pre_state_body — here that value happens to contain the connector's own
        // resolved credential (a server reflecting it back in a third-party field, the same
        // shape M3 already guards for `pre_state`/`post_state`). Before the fix this leaked
        // verbatim into `compensation_plan`.
        let (db, _d) = db().await;
        let secret = "sk-LEAK-42".to_string();
        let mock = MockExecutor::new(
            vec![ExecOutcome::Ok {
                returned_id: None,
                body: json!({}),
            }],
            vec![
                // pre-state: the field about to be overwritten currently holds the secret.
                Some(json!({"note": format!("issued with {secret}"), "write_date": "v1"})),
                // post-state read-back.
                Some(json!({"note": "updated note", "write_date": "v2"})),
            ],
        );
        let exec = Arc::new(SecretMockExecutor { inner: mock, secret: secret.clone() });
        let tool = HttpConnectorTool {
            manifest: full_manifest(),
            op: update_op_spec(),
            executor: exec,
            kill: Arc::new(AtomicBool::new(false)),
            cred_ref: "odoo.api_key".into(),
            manifest_hash: "test-hash".into(),
        };
        let (ctx, _kd) = ctx(db.clone()).await;
        tool.execute(
            json!({"params": {"ids": [7], "values": {"note": "updated note"}}}),
            &ctx,
        )
        .await
        .unwrap();

        let rows = journal::list_by_session(&db, &ctx.session_id.to_string())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let plan = rows[0].compensation_plan.as_deref().unwrap();
        assert!(
            !plan.contains(&secret),
            "compensation_plan must not contain the resolved secret: {plan}"
        );
        assert!(
            plan.contains("issued with"),
            "non-secret restored text must be preserved: {plan}"
        );
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
            manifest_hash: "test-hash".into(),
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
            manifest_hash: "test-hash".into(),
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
            manifest_hash: "test-hash".into(),
        };
        assert_eq!(tool.risk_tier(&json!({})), RiskTier::IrreversibleWrite);
    }

    // ---- M6a credential pre-flight ------------------------------------------------------

    #[tokio::test]
    async fn credential_preflight_none_when_manifest_declares_no_auth() {
        let m = Arc::new(
            manifest::parse(
                r#"{"connector_name":"c","version":"1","base_url":"https://x.example.com",
                    "allowed_ip_cidrs":[],"ops":[]}"#,
            )
            .unwrap(),
        );
        let exec = HttpExecutor::new(HttpExecutorConfig::production(
            m,
            Arc::new(AtomicBool::new(false)),
            std::time::Duration::from_secs(5),
        ));
        assert_eq!(
            exec.credential_preflight().await,
            None,
            "nothing to preflight when the manifest declares no auth section"
        );
    }

    #[tokio::test]
    async fn credential_preflight_false_when_auth_declared_but_no_getter_configured() {
        let m = Arc::new(
            manifest::parse(
                r#"{"connector_name":"c","version":"1","base_url":"https://x.example.com",
                    "allowed_ip_cidrs":[],"ops":[],
                    "auth":{"scheme":"bearer","cred_ref":"c.key"}}"#,
            )
            .unwrap(),
        );
        // No `with_credential_getter` call — `production()` defaults it to `None`.
        let exec = HttpExecutor::new(HttpExecutorConfig::production(
            m,
            Arc::new(AtomicBool::new(false)),
            std::time::Duration::from_secs(5),
        ));
        assert_eq!(
            exec.credential_preflight().await,
            Some(false),
            "auth declared but no getter configured must preflight as unavailable"
        );
    }

    #[tokio::test]
    async fn credential_preflight_true_when_getter_resolves_the_secret() {
        let m = Arc::new(
            manifest::parse(
                r#"{"connector_name":"c","version":"1","base_url":"https://x.example.com",
                    "allowed_ip_cidrs":[],"ops":[],
                    "auth":{"scheme":"bearer","cred_ref":"c.key"}}"#,
            )
            .unwrap(),
        );
        let mut map = std::collections::HashMap::new();
        map.insert("c.key".to_string(), "sk-resolvable".to_string());
        let getter = Arc::new(crate::connector::credential::mock::MockCredentialGetter(map));
        let exec = HttpExecutor::new(
            HttpExecutorConfig::production(m, Arc::new(AtomicBool::new(false)), std::time::Duration::from_secs(5))
                .with_credential_getter(Some(getter)),
        );
        assert_eq!(
            exec.credential_preflight().await,
            Some(true),
            "a resolvable credential must preflight as available"
        );
    }

    // ---- C1 host-scoping + m1 query-param stripping (unit-level) ----------------------

    #[test]
    fn hop_host_matches_normalizes_case_and_trailing_dot_but_ignores_port_and_path() {
        assert!(hop_host_matches("https://Example.com:8443/x", "example.com"));
        assert!(hop_host_matches("https://example.com./y", "example.com"));
        // Same host, different port/path/scheme — still a match (C1 is host-only).
        assert!(hop_host_matches("http://example.com/other", "example.com"));
        assert!(!hop_host_matches("https://attacker.example/x", "example.com"));
        // Unparseable hop URL fails closed to "no match" rather than panicking.
        assert!(!hop_host_matches("not a url", "example.com"));
    }

    #[test]
    fn strip_query_param_removes_only_the_named_param() {
        assert_eq!(
            strip_query_param("http://h/x?api_key=stale&other=1", "api_key"),
            "http://h/x?other=1"
        );
        // Removing the only param drops the `?` entirely, not just its value.
        assert_eq!(strip_query_param("http://h/x?api_key=stale", "api_key"), "http://h/x");
        // No matching param — URL unchanged (aside from normalization round-trip).
        assert_eq!(strip_query_param("http://h/x?other=1", "api_key"), "http://h/x?other=1");
        // Unparseable input is returned as-is.
        assert_eq!(strip_query_param("not a url", "api_key"), "not a url");
    }

    #[test]
    fn hop_target_url_strips_only_for_query_param_scheme_on_a_matching_host() {
        let base_host = "example.com".to_string();
        let query_auth = Some((ResolvedAuthScheme::QueryParam("api_key".to_string()), "S".to_string()));
        // Matching host + query-param scheme: the stale param is stripped.
        assert_eq!(
            hop_target_url("http://example.com/x?api_key=stale", &query_auth, &base_host),
            "http://example.com/x"
        );
        // Cross-host: the hop URL passes through untouched even though the scheme is
        // query-param — C1 host-scoping applies uniformly to the auth PATH, not just the
        // header/param application.
        assert_eq!(
            hop_target_url("http://attacker.example/x?api_key=stale", &query_auth, &base_host),
            "http://attacker.example/x?api_key=stale"
        );
        // A non-query-param scheme never triggers stripping, even on the matching host.
        let bearer_auth = Some((ResolvedAuthScheme::Bearer, "S".to_string()));
        assert_eq!(
            hop_target_url("http://example.com/x?api_key=stale", &bearer_auth, &base_host),
            "http://example.com/x?api_key=stale"
        );
        // No auth at all: unchanged.
        assert_eq!(
            hop_target_url("http://example.com/x?api_key=stale", &None, &base_host),
            "http://example.com/x?api_key=stale"
        );
    }
}
