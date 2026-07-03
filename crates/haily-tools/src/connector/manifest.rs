//! Manifest schema — the human-approved declaration a `HttpConnectorTool` interprets.
//!
//! A manifest is stored as `manifest_json` in `connector_manifests` (migration 0013),
//! parsed here at startup, and NEVER executed as code (no codegen). Each `OpSpec` declares
//! its own `risk_tier` so the gating tier travels WITH the approved schema; a missing or
//! unrecognized tier FAIL-CLOSES to `IrreversibleWrite` (the fail-closed contract on
//! `RiskTier` — an op whose blast radius is unknown must be treated as the worst case).
use crate::RiskTier;
use serde::Deserialize;

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

/// Parse a stored `manifest_json` string into a `Manifest`.
///
/// # Errors
/// Returns an error if the JSON is malformed or missing required fields — the caller
/// (startup registration) SKIPS an unparseable manifest rather than registering a tool
/// that would fail-closed on every call.
pub fn parse(manifest_json: &str) -> anyhow::Result<Manifest> {
    serde_json::from_str(manifest_json)
        .map_err(|e| anyhow::anyhow!("connector manifest parse failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn op(risk_tier: Option<&str>) -> OpSpec {
        OpSpec {
            name: "odoo_write".into(),
            model: Some("res.partner".into()),
            method: Some("write".into()),
            risk_tier: risk_tier.map(str::to_string),
            compensability: Some("compensatable".into()),
            compensation: Some(json!({"op": "write"})),
            correlation_field: Some("ref".into()),
        }
    }

    #[test]
    fn risk_tier_parses_declared_values() {
        assert_eq!(op(Some("Read")).risk_tier(), RiskTier::Read);
        assert_eq!(
            op(Some("ReversibleWrite")).risk_tier(),
            RiskTier::ReversibleWrite
        );
        assert_eq!(
            op(Some("IrreversibleWrite")).risk_tier(),
            RiskTier::IrreversibleWrite
        );
        assert_eq!(op(Some("Blocked")).risk_tier(), RiskTier::Blocked);
    }

    #[test]
    fn risk_tier_fail_closes_on_absent_or_unknown() {
        // Absent tier → IrreversibleWrite (unresolvable = worst case).
        assert_eq!(op(None).risk_tier(), RiskTier::IrreversibleWrite);
        // Unrecognized string → IrreversibleWrite (malformed = worst case).
        assert_eq!(op(Some("Cheap")).risk_tier(), RiskTier::IrreversibleWrite);
        assert_eq!(op(Some("")).risk_tier(), RiskTier::IrreversibleWrite);
    }

    #[test]
    fn parse_valid_manifest_with_ops() {
        let json = r#"{
            "connector_name": "odoo",
            "version": "1",
            "base_url": "https://erp.example.com",
            "allowed_ip_cidrs": ["93.184.216.34/32"],
            "ops": [
                {"name":"odoo_contact_create","model":"res.partner","method":"create",
                 "risk_tier":"IrreversibleWrite","compensability":"compensatable",
                 "compensation":{"op":"unlink"}}
            ]
        }"#;
        let m = parse(json).unwrap();
        assert_eq!(m.connector_name, "odoo");
        assert_eq!(m.allowed_ip_cidrs, vec!["93.184.216.34/32"]);
        assert_eq!(m.ops.len(), 1);
        assert_eq!(m.ops[0].name, "odoo_contact_create");
        assert!(m.ops[0].is_create());
    }

    #[test]
    fn parse_rejects_malformed_json() {
        assert!(parse("not json {{{").is_err());
        assert!(parse(r#"{"version":"1"}"#).is_err()); // missing required fields
    }

    #[test]
    fn compensability_defaults_to_final() {
        let mut o = op(None);
        o.compensability = None;
        assert_eq!(o.compensability_str(), "final");
    }
}
