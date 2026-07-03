//! `OdooExecutor` — the Odoo-specific `ConnectorExecutor` (phase 5, R3 terminal proof).
//!
//! Specializes the generic phase-4 substrate to Odoo's external JSON-RPC API: every write
//! is an `execute_kw(db, uid, key, model, method, args, kwargs)` POST to `/jsonrpc`, with an
//! EXPLICIT `context:{lang, tz}` on every call (so server locale never silently shifts a
//! result), a client `correlation_ref` embedded in create payloads (C7 read-back), and
//! client-side pre-validation of required fields.
//!
//! Security contract:
//! - **C4** — the API key is read from `kms_preferences` (`<cred_ref>`, e.g.
//!   `connector.odoo.api_key`) BY REFERENCE at call time. It is never copied into a struct
//!   field, never journaled, never logged. Any request shape that could be logged has its
//!   `key`/`Authorization`/`Cookie` stripped via [`redact`] first.
//! - **M4/M7** — a server fault is classified from the STRUCTURED `error.data.name`
//!   (see [`odoo_fault`]), never the human message; unrecognized → fail-closed non-retryable.
//! - **C5** — the human `faultString` is tag-stripped before it reaches `ExecOutcome::Fault`.
//! - **C7** — a reqwest transport/timeout failure surfaces as `Err` (NOT a `Fault`), so the
//!   caller reads back by `correlation_ref` rather than concluding the write failed.
//! - **C10** — `read_back` surfaces `write_date` so the undo path can version-guard.
//!
//! Compensation routing: the phase-3 `journal_undo` logic drives `call`/`read_back` with the
//! compensation plan's `op` keyword (`write`/`unlink`), NOT the original manifest op NAME. So
//! every manifest `compensation` template carries its own `model` (+`method`), and the
//! executor resolves model/method from the PLAN for a compensation op (from the manifest for a
//! primary op) — fail-closed when neither yields a model.
use crate::connector::executor::{ConnectorExecutor, ExecOutcome};
use crate::connector::manifest::Manifest;
use crate::connector::odoo_fault::{self, FaultClass};
use crate::connector::redact;
use crate::security;
use anyhow::{Context, Result};
use async_trait::async_trait;
use haily_db::queries::meta;
use haily_db::DbHandle;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Odoo x2many field write command codes (`odoo/fields.py::Command`). Verified 2026-07-03
/// against the Odoo 18.0 ORM reference. Used by [`encode_command`] so a caller can express
/// a relational (m2m/o2o) mutation without hand-rolling the numeric tuple.
///
/// [UNVERIFIED — cross-check against the pinned image's `odoo/fields.py Command` docstring
/// if a future Odoo release renumbers them; the values below are stable across 13.0–18.0.]
pub mod command {
    pub const CREATE: i64 = 0;
    pub const UPDATE: i64 = 1;
    pub const DELETE: i64 = 2;
    pub const UNLINK: i64 = 3;
    pub const LINK: i64 = 4;
    pub const CLEAR: i64 = 5;
    pub const SET: i64 = 6;
}

/// Build an Odoo x2many command tuple `[code, id, values]`. `id`/`values` are elided to `0`
/// where the command ignores them (CLEAR/SET use `0` for id; LINK/UNLINK/DELETE use `0`/
/// `false` for values), matching the ORM's `(code, id_or_0, values_or_0)` shape.
#[must_use]
pub fn encode_command(code: i64, id: i64, values: Value) -> Value {
    json!([code, id, values])
}

/// Odoo executor bound to one approved manifest. Holds the DB handle so the credential is
/// read BY REFERENCE at call time (C4) — the key never lives in this struct. `db_name`/`uid`
/// identify the Odoo database + the scoped service user the key authenticates.
pub struct OdooExecutor {
    manifest: Arc<Manifest>,
    /// Credential preference key (e.g. `connector.odoo.api_key`) — a REFERENCE, not the key.
    cred_ref: String,
    /// DB handle used only to read the credential preference at call time (C4).
    db: Arc<DbHandle>,
    /// Odoo database name (the `db` positional of `execute_kw`).
    db_name: String,
    /// Authenticated user id (the `uid` positional). The scoped service user, not admin.
    uid: i64,
    /// Locale context sent EXPLICITLY on every call so server defaults never shift a result.
    lang: String,
    tz: String,
    kill: Arc<AtomicBool>,
    timeout: Duration,
    /// TEST ONLY — never true in production. When true, the SSRF allowance permits a LOOPBACK
    /// address that also matches a pinned CIDR, so the golden suite can reach a local Odoo
    /// sandbox at `127.0.0.1:8069`. It NEVER relaxes the metadata/link-local surface. The
    /// production wiring (`Orchestrator::init`) constructs `HttpExecutor` with no loopback
    /// allowance at all; ONLY the golden test builds an `OdooExecutor` with this set true.
    allow_loopback: bool,
}

/// Parameters for constructing an [`OdooExecutor`]. Grouped so the constructor stays within
/// a sane arity and the call-site reads as one struct literal.
pub struct OdooExecutorConfig {
    pub manifest: Arc<Manifest>,
    pub cred_ref: String,
    pub db: Arc<DbHandle>,
    pub db_name: String,
    pub uid: i64,
    pub lang: String,
    pub tz: String,
    pub kill: Arc<AtomicBool>,
    pub timeout: Duration,
    /// TEST ONLY — never true in production. See [`OdooExecutor::allow_loopback`]. Defaults to
    /// `false` via [`OdooExecutorConfig::production`]; only the golden test sets it true.
    pub allow_loopback: bool,
}

impl OdooExecutorConfig {
    /// Build a production config with the loopback SSRF carve-out DISABLED. Use this at every
    /// non-test construction site so the TEST-ONLY `allow_loopback` cannot be set by accident.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn production(
        manifest: Arc<Manifest>,
        cred_ref: String,
        db: Arc<DbHandle>,
        db_name: String,
        uid: i64,
        lang: String,
        tz: String,
        kill: Arc<AtomicBool>,
        timeout: Duration,
    ) -> Self {
        Self {
            manifest,
            cred_ref,
            db,
            db_name,
            uid,
            lang,
            tz,
            kill,
            timeout,
            allow_loopback: false,
        }
    }
}

impl OdooExecutor {
    /// Build an executor from its config. The credential is NOT read here — only at call
    /// time (C4), so a rotated key is picked up without reconstructing the executor.
    #[must_use]
    pub fn new(cfg: OdooExecutorConfig) -> Self {
        Self {
            manifest: cfg.manifest,
            cred_ref: cfg.cred_ref,
            db: cfg.db,
            db_name: cfg.db_name,
            uid: cfg.uid,
            lang: cfg.lang,
            tz: cfg.tz,
            kill: cfg.kill,
            timeout: cfg.timeout,
            allow_loopback: cfg.allow_loopback,
        }
    }

    /// Resolve `(model, method)` for a call. `op` is EITHER a manifest op NAME (the primary
    /// write, e.g. `odoo_contact_update`) OR a bare compensation-op keyword the undo logic
    /// passes from the plan (`write`/`unlink`/`archive`/`compensate`). For the latter the
    /// model+method are carried on `params` (the compensation plan enriched with `model` at
    /// manifest-approval time), since a compensation op is not itself a manifest op.
    ///
    /// Fail-closed: a manifest op with no declared model/method, or a compensation op with no
    /// model on the params, is an error (never a guessed model).
    fn op_model_method(&self, op: &str, params: &Value) -> Result<(String, String)> {
        if let Some(spec) = self.manifest.ops.iter().find(|o| o.name == op) {
            let model = spec
                .model
                .clone()
                .with_context(|| format!("op '{op}' has no model"))?;
            let method = spec
                .method
                .clone()
                .with_context(|| format!("op '{op}' has no method"))?;
            return Ok((model, method));
        }
        // Compensation op (not a manifest op NAME): model+method travel on the plan/params.
        let model = params
            .get("model")
            .and_then(Value::as_str)
            .with_context(|| format!("compensation op '{op}' has no model on its plan"))?
            .to_string();
        // `method` defaults from the compensation keyword: archive/write → `write`,
        // unlink → `unlink`, read → `search_read`.
        let method = params
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| match op {
                "unlink" | "delete" => "unlink".to_string(),
                "read" => "search_read".to_string(),
                _ => "write".to_string(),
            });
        Ok((model, method))
    }

    /// Read the actual API key from `kms_preferences` by reference (C4). The returned key is
    /// used only to build the request body and is dropped immediately after — it is NEVER
    /// stored on `self`, logged, or journaled.
    async fn read_key(&self) -> Result<String> {
        meta::get_preference(&self.db, &self.cred_ref)
            .await?
            .filter(|k| !k.is_empty())
            .with_context(|| {
                format!(
                    "Odoo credential '{}' not configured in preferences",
                    self.cred_ref
                )
            })
    }

    /// The `/jsonrpc` endpoint under the approved base_url (trailing slash tolerated).
    fn endpoint(&self) -> String {
        format!("{}/jsonrpc", self.manifest.base_url.trim_end_matches('/'))
    }

    /// Client-side pre-validation of required fields (fail fast before any network call).
    /// `res.partner` requires `name`; `crm.lead` requires `name` + `type`. A missing field
    /// is a caller error, surfaced as `Err` (not a fault) — nothing was sent to Odoo.
    fn prevalidate(model: &str, values: &Value) -> Result<()> {
        let require = |field: &str| -> Result<()> {
            let present = values
                .get(field)
                .map(|v| !v.is_null() && v != &json!(""))
                .unwrap_or(false);
            if present {
                Ok(())
            } else {
                anyhow::bail!("Odoo {model}.create requires '{field}'")
            }
        };
        match model {
            "res.partner" => require("name"),
            "crm.lead" => {
                require("name")?;
                require("type")
            }
            _ => Ok(()),
        }
    }

    /// Build the `execute_kw` positional `args` for a create: `[values]` with the
    /// `correlation_ref` embedded into the model's correlation field so the write is
    /// discoverable on read-back even if the response is lost (C7). Pre-validates required
    /// fields first.
    ///
    /// `correlation_field` is the model's writable field that holds the ref (e.g. `ref` on
    /// `res.partner`/`crm.lead`). It is `None` for a model with NO such field (e.g.
    /// `mail.activity` has no `ref`) — in that case NOTHING is injected, because writing an
    /// unknown field makes Odoo reject the whole create. A create with no correlation field
    /// is recoverable only by its returned id (a lost response falls back to `unverified`).
    fn create_args(
        model: &str,
        values: &Value,
        correlation_ref: &str,
        correlation_field: Option<&str>,
    ) -> Result<Value> {
        Self::prevalidate(model, values)?;
        let mut vals = values.clone();
        if let (Some(field), Some(obj)) = (correlation_field, vals.as_object_mut()) {
            obj.entry(field)
                .or_insert_with(|| Value::String(correlation_ref.to_string()));
        }
        Ok(json!([vals]))
    }

    /// Resolve the correlation field name for `model` from the manifest: the FIRST op declaring
    /// this model with a `correlation_field`. `None` when the model has no such field declared
    /// (e.g. `mail.activity`) — the executor then neither writes nor searches by a ref for it.
    fn correlation_field_for(&self, model: &str) -> Option<String> {
        self.manifest
            .ops
            .iter()
            .filter(|o| o.model.as_deref() == Some(model))
            .find_map(|o| o.correlation_field.clone())
    }

    /// POST an `execute_kw` call and return the raw parsed JSON-RPC response body. A
    /// transport/timeout failure is an `Err` (C7 signal); a reachable server that returned a
    /// structured fault is `Ok` with an `error` object the caller classifies.
    async fn rpc(&self, model: &str, method: &str, args: Value, kwargs: Value) -> Result<Value> {
        let key = self.read_key().await?;
        // execute_kw positional args: [db, uid, key, model, method, args, kwargs].
        // The key is inserted ONLY into this transient body — never persisted (C4).
        let payload = json!({
            "jsonrpc": "2.0",
            "method": "call",
            "id": null,
            "params": {
                "service": "object",
                "method": "execute_kw",
                "args": [self.db_name, self.uid, key, model, method, args, kwargs],
            }
        });
        let body = serde_json::to_string(&payload)
            .map_err(|e| anyhow::anyhow!("odoo rpc: serialize failed: {e}"))?;

        let resp = security::follow_redirects_with_guard_allowance_loopback(
            &self.endpoint(),
            &self.manifest.allowed_ip_cidrs,
            self.timeout,
            self.allow_loopback,
            move |client, url| {
                client
                    .post(url)
                    .header("content-type", "application/json")
                    .body(body.clone())
            },
        )
        .await
        // A transport/timeout failure MUST bubble as Err so the caller reads back (C7) —
        // do NOT swallow it into a fault. The URL is the approved base_url (no secret).
        .with_context(|| format!("odoo rpc transport failure to {}", self.endpoint()))?;

        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("odoo rpc: body read failed: {e}"))?;
        serde_json::from_str::<Value>(&text)
            .map_err(|e| anyhow::anyhow!("odoo rpc: response not JSON: {e}"))
    }

    /// The kwargs object carried on every call: an EXPLICIT locale context so a server-side
    /// default locale can never silently change a computed/translated result.
    fn base_kwargs(&self) -> Value {
        json!({ "context": { "lang": self.lang, "tz": self.tz } })
    }

    /// Turn a raw JSON-RPC response body into an `ExecOutcome`. A structured `error` object
    /// is classified (M7) and returned as `ExecOutcome::Fault` with the tag-stripped human
    /// string (C5); a `result` is `ExecOutcome::Ok`, extracting an integer/string id.
    fn outcome_from_body(body: Value) -> ExecOutcome {
        if let Some(fault) = odoo_fault::extract_fault(&body) {
            let class = odoo_fault::classify(&fault);
            // C5: neutralize any tool-protocol tag in the third-party message before it can
            // reach a journal row or an LLM. Classification already used only data.name (M7).
            let safe = redact::strip_tool_tags(&fault.fault_string);
            // The retry logic keys on the structured code/name; we forward the CLASS as the
            // code so downstream matching is on our normalized token, not the raw path.
            return ExecOutcome::Fault {
                fault_string: safe,
                code: Some(fault_class_token(class).to_string()),
                name: fault.name,
            };
        }
        let result = body.get("result").cloned().unwrap_or(Value::Null);
        let returned_id = extract_id(&result);
        ExecOutcome::Ok {
            returned_id,
            body: result,
        }
    }
}

/// Stable string token for a `FaultClass`, forwarded as the `ExecOutcome::Fault` code so the
/// undo/retry state machine matches on a normalized value (e.g. `MissingError` triggers the
/// already-done path) rather than the raw qualified path.
fn fault_class_token(class: FaultClass) -> &'static str {
    match class {
        FaultClass::NonRetryableAccess => "AccessError",
        FaultClass::RetryableValidation => "ValidationError",
        FaultClass::StaleReference => "MissingError",
        FaultClass::Unknown => "UnknownError",
    }
}

/// Extract a record id from an Odoo `create`/`search`/`read` result. A `create` returns the
/// new integer id; `search` returns `[ids]`; `read` returns `[{...}]`. Returns the first id
/// found as a string, or `None`.
fn extract_id(result: &Value) -> Option<String> {
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

#[async_trait]
impl ConnectorExecutor for OdooExecutor {
    async fn call(&self, op: &str, params: &Value) -> Result<ExecOutcome> {
        // M5 (second line of defense, mirrors HttpExecutor): a caller bypassing the tool's
        // own re-check still cannot write past a live kill switch. Acquire orders after any
        // relaxed store of the flag.
        if self.kill.load(Ordering::Acquire) {
            anyhow::bail!("kill switch engaged — odoo call '{op}' blocked");
        }

        let (model, method) = self.op_model_method(op, params)?;
        let correlation_ref = params
            .get("correlation_ref")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let values = params.get("values").cloned().unwrap_or(Value::Null);

        // Build execute_kw positional args per method. `write(ids, vals)` prefers a BATCH
        // over per-record calls; `create([vals])` embeds the correlation_ref (C7).
        let args = match method.as_str() {
            "create" => {
                let field = self.correlation_field_for(&model);
                Self::create_args(&model, &values, &correlation_ref, field.as_deref())?
            }
            "write" => {
                let ids = params
                    .get("ids")
                    .cloned()
                    .or_else(|| params.get("id").map(|id| json!([id])))
                    .unwrap_or(Value::Null);
                json!([ids, values])
            }
            "unlink" | "read" | "action_feedback" => {
                let ids = params
                    .get("ids")
                    .cloned()
                    .or_else(|| params.get("id").map(|id| json!([id])))
                    .unwrap_or(Value::Null);
                json!([ids])
            }
            // Any other declared method: pass an explicit `args` array through verbatim.
            _ => params.get("args").cloned().unwrap_or(json!([])),
        };

        let body = self.rpc(&model, &method, args, self.base_kwargs()).await?;
        Ok(Self::outcome_from_body(body))
    }

    async fn read_back(
        &self,
        op: &str,
        correlation_ref: &str,
        model_hint: Option<&str>,
        id_hint: Option<&str>,
    ) -> Result<Value> {
        // read_back receives EITHER a manifest op NAME (post-write verify) or a bare
        // compensation-op keyword (undo pre-check). The model is resolved from the manifest for
        // the former; for the latter the undo logic passes the compensation plan's `model` as
        // `model_hint` so we query the CORRECT model (a `mail.activity` unlink must read back
        // `mail.activity`, NOT the manifest's first model `res.partner`).
        let model = match self.op_model_method(op, &Value::Null) {
            Ok((model, _method)) => model,
            // A bare compensation op (write/unlink) with no manifest entry: use the caller's
            // model hint (the compensation plan's model). Fall back to the manifest's first
            // model ONLY if no hint was supplied — a legacy plan with no model — and fail
            // closed if the manifest itself declares none.
            Err(_) => match model_hint {
                Some(m) => m.to_string(),
                None => self
                    .manifest
                    .ops
                    .iter()
                    .find_map(|o| o.model.clone())
                    .with_context(|| {
                        format!("read_back '{op}': no model hint and none resolvable from manifest")
                    })?,
            },
        };
        // Locate the record by its concrete `id_hint` FIRST when known — an exact id is the
        // strongest locator and is the ONLY option for a record whose ref was never embedded
        // (an UPDATE's forward write does not write a correlation field) or whose model has no
        // correlation field (`mail.activity`). Fall back to the model's correlation field (C7
        // lost-response recovery: a create writes the ref, so it is discoverable by ref even
        // with no id in hand). With neither, the domain is empty → Odoo returns nothing = record
        // not locatable (caller marks `unverified`). The correlation field is requested in
        // `fields` ONLY when the model has one — asking for `ref` on `mail.activity` faults.
        let corr_field = self.correlation_field_for(&model);
        let domain: Value = match (id_hint, corr_field.as_deref(), correlation_ref.is_empty()) {
            (Some(id), _, _) => {
                let id_num = id.parse::<i64>().map(Value::from).unwrap_or(Value::Null);
                json!([[["id", "=", id_num]]])
            }
            (None, Some(field), false) => json!([[[field, "=", correlation_ref]]]),
            _ => json!([]),
        };
        // No explicit `fields`: return ALL readable fields. This guarantees every field the
        // caller wrote is present for the diff (`request_fields_match`) AND never requests a
        // field the model lacks — asking for `ref`/`name`/`active` on `mail.activity` (which
        // has none of them) would itself fault. `write_date` is always present (C10).
        // `active_test:false` returns archived records too, so a create-undo (archive) /
        // archive-undo read-back still finds the record it just flipped inactive.
        let kwargs = json!({
            "limit": 1,
            "context": { "lang": self.lang, "tz": self.tz, "active_test": false },
        });
        let body = self
            .rpc(&model, "search_read", domain, kwargs)
            .await
            .with_context(|| format!("odoo read_back '{op}' transport failure"))?;
        // A fault on read-back is surfaced as an Err so the caller marks the row unverified
        // (does NOT block a later undo) — read-back never returns a partial fault shape.
        if let Some(fault) = odoo_fault::extract_fault(&body) {
            let safe = redact::strip_tool_tags(&fault.fault_string);
            anyhow::bail!("odoo read_back '{op}' faulted: {safe}");
        }
        let result = body.get("result").cloned().unwrap_or(Value::Null);
        // search_read returns `[{...}]`; unwrap the first record for the caller's diff.
        Ok(match result {
            Value::Array(mut arr) if !arr.is_empty() => arr.remove(0),
            other => other,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encode_command_shapes_x2many_tuple() {
        // Command tuples are `[code, id, values]` per the Odoo ORM.
        assert_eq!(
            encode_command(command::LINK, 7, json!(0)),
            json!([command::LINK, 7, 0])
        );
        assert_eq!(command::CREATE, 0);
        assert_eq!(command::SET, 6);
        // SET replaces the whole set: `(6, 0, [ids])`.
        assert_eq!(
            encode_command(command::SET, 0, json!([1, 2, 3])),
            json!([6, 0, [1, 2, 3]])
        );
    }

    #[test]
    fn prevalidate_enforces_required_fields() {
        // res.partner requires name.
        assert!(OdooExecutor::prevalidate("res.partner", &json!({"name": "Alice"})).is_ok());
        assert!(OdooExecutor::prevalidate("res.partner", &json!({"email": "a@b.c"})).is_err());
        // crm.lead requires name AND type.
        assert!(
            OdooExecutor::prevalidate("crm.lead", &json!({"name": "L", "type": "lead"})).is_ok()
        );
        assert!(OdooExecutor::prevalidate("crm.lead", &json!({"name": "L"})).is_err());
        // An empty-string required field is treated as absent.
        assert!(OdooExecutor::prevalidate("res.partner", &json!({"name": ""})).is_err());
        // An unknown model has no client-side requirements.
        assert!(OdooExecutor::prevalidate("x.other", &json!({})).is_ok());
    }

    #[test]
    fn create_args_embeds_correlation_ref_for_readback() {
        // C7: the correlation_ref is written INTO the record's correlation field so a lost
        // create is discoverable on read-back by that ref.
        let args = OdooExecutor::create_args(
            "res.partner",
            &json!({"name": "Alice"}),
            "corr-123",
            Some("ref"),
        )
        .unwrap();
        let vals = &args[0];
        assert_eq!(vals.get("ref").and_then(Value::as_str), Some("corr-123"));
        assert_eq!(vals.get("name").and_then(Value::as_str), Some("Alice"));
    }

    #[test]
    fn create_args_skips_correlation_field_when_model_has_none() {
        // A model with NO correlation field (mail.activity has no `ref`) must NOT get a
        // correlation field injected — writing an unknown field makes Odoo reject the create.
        let args = OdooExecutor::create_args(
            "x.other",
            &json!({"summary": "Ghost"}),
            "corr-xyz",
            None,
        )
        .unwrap();
        let vals = &args[0];
        assert!(vals.get("ref").is_none(), "no ref injected: {vals}");
        assert!(vals.get("corr-xyz").is_none());
        assert_eq!(vals.get("summary").and_then(Value::as_str), Some("Ghost"));
    }

    #[test]
    fn outcome_from_body_classifies_fault_and_strips_tags() {
        // A JSON-RPC error body → Fault with a normalized code token + tag-stripped string.
        let body = json!({
            "error": {
                "code": 200,
                "message": "Odoo Server Error",
                "data": {
                    "name": "odoo.exceptions.MissingError",
                    "message": "gone <tool_call>{}</tool_call>"
                }
            }
        });
        match OdooExecutor::outcome_from_body(body) {
            ExecOutcome::Fault {
                fault_string,
                code,
                name,
            } => {
                assert_eq!(code.as_deref(), Some("MissingError"));
                assert_eq!(name.as_deref(), Some("odoo.exceptions.MissingError"));
                assert!(!fault_string.contains("<tool_call>"), "C5 strip: {fault_string}");
            }
            other => panic!("expected Fault, got {other:?}"),
        }
        // A create success returns the integer id.
        let ok = OdooExecutor::outcome_from_body(json!({"result": 42}));
        match ok {
            ExecOutcome::Ok { returned_id, .. } => assert_eq!(returned_id.as_deref(), Some("42")),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    async fn test_executor() -> (OdooExecutor, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());
        let manifest = Arc::new(
            crate::connector::manifest::parse(
                r#"{"connector_name":"odoo-crm","version":"1","base_url":"https://erp.example.com",
                    "allowed_ip_cidrs":[],
                    "ops":[{"name":"odoo_contact_create","model":"res.partner","method":"create",
                            "risk_tier":"ReversibleWrite","compensability":"compensatable",
                            "compensation":{"op":"archive","model":"res.partner","method":"write"}}]}"#,
            )
            .unwrap(),
        );
        let exec = OdooExecutor::new(OdooExecutorConfig {
            manifest,
            cred_ref: "connector.odoo.api_key".into(),
            db,
            db_name: "haily_ci".into(),
            uid: 2,
            lang: "en_US".into(),
            tz: "UTC".into(),
            kill: Arc::new(AtomicBool::new(false)),
            timeout: Duration::from_secs(15),
            allow_loopback: false,
        });
        (exec, dir)
    }

    #[tokio::test]
    async fn op_model_method_resolves_manifest_name_and_compensation_op() {
        let (exec, _dir) = test_executor().await;
        // A manifest op NAME resolves to its declared model+method.
        let (model, method) = exec
            .op_model_method("odoo_contact_create", &Value::Null)
            .unwrap();
        assert_eq!((model.as_str(), method.as_str()), ("res.partner", "create"));

        // A bare compensation op keyword resolves model+method from the PLAN (not the
        // manifest — it is not a manifest op NAME). This is the seam the phase-3 undo logic
        // drives: it passes the plan's `op` (`write`/`unlink`), not the original op name.
        let plan = json!({"op": "write", "model": "res.partner", "method": "write", "ids": [7]});
        let (cm, cmethod) = exec.op_model_method("write", &plan).unwrap();
        assert_eq!((cm.as_str(), cmethod.as_str()), ("res.partner", "write"));

        // A compensation op with NO model on its plan fails closed (never a guessed model).
        assert!(exec.op_model_method("write", &json!({"op": "write"})).is_err());

        // The method DEFAULTS by keyword when the plan omits it: unlink → unlink.
        let (_m, um) = exec
            .op_model_method("unlink", &json!({"model": "mail.activity"}))
            .unwrap();
        assert_eq!(um, "unlink");
    }

    #[test]
    fn extract_id_handles_create_search_read_shapes() {
        assert_eq!(extract_id(&json!(7)), Some("7".into())); // create → int
        assert_eq!(extract_id(&json!([9, 10])), Some("9".into())); // search → [ids]
        assert_eq!(extract_id(&json!([{"id": 5, "name": "x"}])), Some("5".into())); // read
        assert_eq!(extract_id(&json!([])), None);
        assert_eq!(extract_id(&Value::Null), None);
    }
}
