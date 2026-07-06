//! Manifest schema — the human-approved declaration a `HttpConnectorTool` interprets.
//!
//! A manifest is stored as `manifest_json` in `connector_manifests` (migration 0013),
//! parsed here at startup, and NEVER executed as code (no codegen). Each `OpSpec` declares
//! its own `risk_tier` so the gating tier travels WITH the approved schema; a missing or
//! unrecognized tier FAIL-CLOSES to `IrreversibleWrite` (the fail-closed contract on
//! `RiskTier` — an op whose blast radius is unknown must be treated as the worst case).
//!
//! v2 adds two OPTIONAL declarative sections consumed by later phases, never by this one:
//! `auth` (how `HttpExecutor` authenticates, phase 2) and `protocol` (how it shapes the
//! wire request/response, phase 3). Both are pure data — templates and match tables, never
//! executable code — so a poisoned or over-ambitious manifest can never smuggle in logic.
use crate::RiskTier;
use serde::{Deserialize, Serialize};

/// A parsed connector manifest. Field names match the JSON stored in `manifest_json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub connector_name: String,
    pub version: String,
    pub base_url: String,
    /// Pinned IP/CIDR allowance (C3), captured at approval time. NOT hostnames.
    #[serde(default)]
    pub allowed_ip_cidrs: Vec<String>,
    #[serde(default)]
    pub ops: Vec<OpSpec>,
    /// How `HttpExecutor` authenticates outbound calls (phase 2 consumes this; absent =
    /// no auth, matching today's behavior). Validated fail-closed by [`parse`] — a manifest
    /// with an unresolvable scheme never reaches the registry.
    #[serde(default)]
    pub auth: Option<AuthSpec>,
    /// Declarative wire-protocol shaping (phase 3 consumes this; absent = the executor's
    /// current generic `{"op","params"}` POST body, unchanged).
    #[serde(default)]
    pub protocol: Option<ProtocolSpec>,
}

/// One declared operation. `name` is the tool name exposed to the agent (e.g.
/// `odoo_contact_create`); `model`/`method` are connector-specific routing hints the
/// executor interprets; `risk_tier` is the declared gating tier (fail-closed);
/// `compensability` + `compensation` describe how to undo it (recorded in the journal).
#[derive(Debug, Clone, Deserialize)]
pub struct OpSpec {
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    /// Declared tier string: "Read" | "ReversibleWrite" | "IrreversibleWrite" | "Blocked".
    /// Any other/absent value fail-closes to `IrreversibleWrite` via `risk_tier()`.
    #[serde(default)]
    pub risk_tier: Option<String>,
    /// read | reversible | compensatable | final — recorded into the journal row and used
    /// by `journal_undo` to decide undo refusal. Absent → treated as `final` (worst case).
    #[serde(default)]
    pub compensability: Option<String>,
    /// The compensation template: how to undo this op (e.g. `{"op":"unlink"}`). Copied
    /// verbatim into the journal's `compensation_plan` BEFORE the external call (outbox).
    #[serde(default)]
    pub compensation: Option<serde_json::Value>,
    /// The model field that holds the client `correlation_ref` for C7 lost-response recovery
    /// (e.g. `res.partner`/`crm.lead` have a writable `ref` field). ABSENT means the model has
    /// no such field (e.g. `mail.activity` has no `ref`) — the executor must NOT write a
    /// correlation field into that model's create (Odoo rejects an unknown field) and cannot
    /// read that record back by correlation; a lost activity create then falls back to the
    /// fail-closed `unverified` read-back, recovered by the returned id only.
    #[serde(default)]
    pub correlation_field: Option<String>,
}

impl OpSpec {
    /// Resolve this op's declared `RiskTier`, FAIL-CLOSING to `IrreversibleWrite` when the
    /// tier is absent or unrecognized — an op whose true blast radius cannot be determined
    /// from the manifest must be gated as the worst case, never a cheaper tier.
    pub fn risk_tier(&self) -> RiskTier {
        match self.risk_tier.as_deref() {
            Some("Read") => RiskTier::Read,
            Some("ReversibleWrite") => RiskTier::ReversibleWrite,
            Some("IrreversibleWrite") => RiskTier::IrreversibleWrite,
            Some("Blocked") => RiskTier::Blocked,
            // Absent or any unknown string: unresolvable → fail-closed.
            _ => RiskTier::IrreversibleWrite,
        }
    }

    /// The journal `compensability` string, defaulting to `final` (no undo) when absent —
    /// the safe default so an op without a declared undo path is never treated as
    /// reversible.
    pub fn compensability_str(&self) -> &str {
        self.compensability.as_deref().unwrap_or("final")
    }

    /// `true` when this op creates a new record — no pre_state/version to capture before
    /// the write (there is nothing there yet). Inferred from the compensability being a
    /// `compensatable` create-style op with a `compensation.op == "unlink"`, else from the
    /// op name containing `create`. Conservative: a false negative just captures an empty
    /// pre_state, which is harmless.
    pub fn is_create(&self) -> bool {
        if let Some(comp) = self.compensation.as_ref() {
            if comp.get("op").and_then(|v| v.as_str()) == Some("unlink") {
                return true;
            }
        }
        self.name.contains("create")
    }
}

/// Declarative auth for `HttpExecutor` (phase 2 consumes it; this phase only parses and
/// validates it). `cred_ref` is a NAME only (e.g. `connector.<name>.api_key`) — the secret
/// itself never appears in the manifest, the same C4 discipline every executor follows via
/// `CredentialGetter`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AuthSpec {
    /// "bearer" | "header" | "query-param". Unrecognized → [`AuthSpec::resolve`] errors,
    /// which [`parse`] propagates so the manifest never registers.
    pub scheme: String,
    pub cred_ref: String,
    /// Required when `scheme == "header"` (e.g. `"X-API-Key"`).
    #[serde(default)]
    pub header_name: Option<String>,
    /// Required when `scheme == "query-param"` (e.g. `"api_key"`).
    #[serde(default)]
    pub param_name: Option<String>,
}

/// The fully-validated auth scheme, resolved once by [`AuthSpec::resolve`] so a later
/// consumer (phase 2's `HttpExecutor`) never has to re-check that the name field it needs
/// is present — by construction, this type cannot exist without it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAuthScheme {
    Bearer,
    Header(String),
    QueryParam(String),
}

impl AuthSpec {
    /// Resolve `scheme` against its required name field, FAIL-CLOSING (an `Err`, propagated
    /// by [`parse`] all the way to "this manifest never registers") rather than silently
    /// degrading — a `scheme="header"` with no `header_name` must never resolve to an
    /// unauthenticated request or a request with the secret placed nowhere (m2).
    pub fn resolve(&self) -> anyhow::Result<ResolvedAuthScheme> {
        match self.scheme.as_str() {
            "bearer" => Ok(ResolvedAuthScheme::Bearer),
            "header" => self
                .header_name
                .clone()
                .map(ResolvedAuthScheme::Header)
                .ok_or_else(|| anyhow::anyhow!("auth scheme \"header\" requires header_name")),
            "query-param" => self
                .param_name
                .clone()
                .map(ResolvedAuthScheme::QueryParam)
                .ok_or_else(|| {
                    anyhow::anyhow!("auth scheme \"query-param\" requires param_name")
                }),
            other => anyhow::bail!("unknown auth scheme: {other}"),
        }
    }
}

/// Declarative wire-protocol shaping. Every field is templates/match-tables ONLY — never
/// executable code. Phase 3 defines the placeholder grammar this feeds:
///
/// `envelope`/`methods` templates may reference a FIXED whitelist of `{{token}}`
/// placeholders (e.g. `{{model}}`, `{{method}}`, `{{args}}`, `{{db}}`, `{{uid}}`, `{{key}}`);
/// any other `{{...}}` is a literal, never evaluated. There is NO arithmetic, no
/// conditionals, no loops — pure substitution. A connector that genuinely needs branching
/// logic is the `ConnectorExecutor` escape hatch ("provably inexpressible in config"), not a
/// reason to extend this grammar.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProtocolSpec {
    /// Appended to `base_url` (e.g. `/jsonrpc`). Absent = no suffix (today's behavior).
    #[serde(default)]
    pub endpoint_suffix: Option<String>,
    /// The request body template, filled by placeholder substitution (phase 3).
    #[serde(default)]
    pub envelope: Option<serde_json::Value>,
    /// Per-method arg-shaping templates (e.g. `create` → `[vals]`, `write` → `[ids, values]`).
    #[serde(default)]
    pub methods: Vec<MethodShape>,
    /// Ordered match rules mapping a connector-side error field/value to a normalized fault
    /// token (`AccessError` / `ValidationError` / `MissingError` / `UnknownError`) that the
    /// undo state machine keys on.
    #[serde(default)]
    pub fault_rules: Vec<FaultRule>,
    /// How to read a record back after a write (locate strategy, `active_test`, unwrap).
    #[serde(default)]
    pub readback: Option<ReadbackSpec>,
    /// Explicit locale/context injected on every call (e.g. `{"lang":"vi_VN","tz":"UTC"}`).
    #[serde(default)]
    pub context: Option<serde_json::Value>,
    /// Per-model required-field lists, checked client-side before a write is attempted.
    #[serde(default)]
    pub prevalidate: Vec<ModelRequiredFields>,
}

/// One per-method arg-shaping template (e.g. `method: "create"`, `arg_template: ["{{vals}}"]`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct MethodShape {
    pub method: String,
    pub arg_template: serde_json::Value,
}

/// One fault-classification match rule: when the connector's error payload has
/// `match_field == match_value` (string equality only, no regex/eval), the fault is
/// reported as `normalized`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FaultRule {
    pub match_field: String,
    pub match_value: String,
    pub normalized: String,
}

/// Declarative read-back shape: how to re-fetch a record after a write.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReadbackSpec {
    /// "id" | "correlation_field". Absent = executor default (phase 3 decides).
    #[serde(default)]
    pub locate_by: Option<String>,
    /// Whether to pass `active_test:false` (Odoo-shaped; ignored by connectors without it).
    #[serde(default)]
    pub active_test: Option<bool>,
    /// Whether the read-back response wraps the record in a one-element array/list.
    #[serde(default)]
    pub unwrap_first: bool,
}

/// Client-side required-field check for one model, run before a write is attempted.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelRequiredFields {
    pub model: String,
    pub required_fields: Vec<String>,
}

/// Parse a stored `manifest_json` string into a `Manifest`.
///
/// # Errors
/// Returns an error if the JSON is malformed, missing required fields, or (m2) declares an
/// `auth` section whose scheme cannot be resolved (unknown scheme, or a `header`/
/// `query-param` scheme missing its required name field) — the caller (startup
/// registration) SKIPS an unparseable manifest rather than registering a tool that would
/// fail-closed on every call, or worse, silently send an unauthenticated/misdirected request.
pub fn parse(manifest_json: &str) -> anyhow::Result<Manifest> {
    let manifest: Manifest = serde_json::from_str(manifest_json)
        .map_err(|e| anyhow::anyhow!("connector manifest parse failed: {e}"))?;
    if let Some(auth) = &manifest.auth {
        auth.resolve()
            .map_err(|e| anyhow::anyhow!("connector manifest auth invalid: {e}"))?;
    }
    Ok(manifest)
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
