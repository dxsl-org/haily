//! Manifest schema — the human-approved declaration a `HttpConnectorTool` interprets.
//!
//! A manifest is stored as `manifest_json` in `connector_manifests` (migration 0013),
//! parsed here at startup, and NEVER executed as code (no codegen). Each `OpSpec` declares
//! its own `risk_tier` so the gating tier travels WITH the approved schema; a missing or
//! unrecognized tier FAIL-CLOSES to `IrreversibleWrite` (the fail-closed contract on
//! `RiskTier` — an op whose blast radius is unknown must be treated as the worst case).
use crate::connector::redact::strip_tool_tags;
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

/// A per-op change between two manifest versions, whitelisted-field-only (m1). `op_name`
/// identifies which declared operation changed; the tier/compensability fields are `None`
/// when that field did not change for this op.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpDiff {
    pub op_name: String,
    /// `Some((old, new))` string tier labels when `risk_tier` changed for this op.
    pub risk_tier: Option<(String, String)>,
    /// `Some((old, new))` compensability strings when it changed for this op.
    pub compensability: Option<(String, String)>,
}

/// The whitelisted, structured diff between two manifest versions (m1). ONLY `ops` (by
/// name) / `risk_tier` / `compensability` are compared — never a raw diff of arbitrary
/// `manifest_json` keys, because that document is connector-authored (semi-trusted at
/// best) and this diff is meant to be rendered in an approval surface. Every string field
/// is run through [`strip_tool_tags`] before being placed here, matching the same
/// escaping discipline the C5 fault-string path uses for other untrusted connector text —
/// downstream rendering must still treat these as plain text (Svelte `{expr}` auto-escape,
/// never `{@html}`), this only removes tool-protocol tag tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ManifestDiff {
    /// Op names present in `new` but not `old`.
    pub added_ops: Vec<String>,
    /// Op names present in `old` but not `new`.
    pub removed_ops: Vec<String>,
    /// Ops present in both versions whose `risk_tier` and/or `compensability` changed.
    pub changed_ops: Vec<OpDiff>,
}

impl ManifestDiff {
    /// `true` when nothing whitelisted changed between the two versions — the caller can
    /// skip forcing re-approval in that case (a version bump with no user-visible schema
    /// change, e.g. a base_url/allowed_ip_cidrs-only edit, is out of this diff's scope by
    /// design: those already require a brand-new immutable row per the phase-13 trigger,
    /// and are not blast-radius-relevant the way ops/risk_tier/compensability are).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added_ops.is_empty() && self.removed_ops.is_empty() && self.changed_ops.is_empty()
    }
}

/// Compare two manifest versions and return the whitelisted diff (m1). Pure/offline — no
/// remote fetch, both manifests are already-stored rows the caller loaded (e.g. the
/// previously-approved version vs. the current live one).
#[must_use]
pub fn manifest_diff(old: &Manifest, new: &Manifest) -> ManifestDiff {
    let mut diff = ManifestDiff::default();

    for new_op in &new.ops {
        match old.ops.iter().find(|o| o.name == new_op.name) {
            None => diff.added_ops.push(strip_tool_tags(&new_op.name)),
            Some(old_op) => {
                let risk_tier = diff_field(&old_op.risk_tier, &new_op.risk_tier);
                let compensability = diff_field(&old_op.compensability, &new_op.compensability);
                if risk_tier.is_some() || compensability.is_some() {
                    diff.changed_ops.push(OpDiff {
                        op_name: strip_tool_tags(&new_op.name),
                        risk_tier,
                        compensability,
                    });
                }
            }
        }
    }
    for old_op in &old.ops {
        if !new.ops.iter().any(|o| o.name == old_op.name) {
            diff.removed_ops.push(strip_tool_tags(&old_op.name));
        }
    }

    diff
}

/// `Some((old, new))`, tag-stripped, when the two optional string fields differ (treating
/// absent as the literal `"(none)"` label so a field that newly appears/disappears is
/// still surfaced instead of silently comparing `None == None`).
fn diff_field(old: &Option<String>, new: &Option<String>) -> Option<(String, String)> {
    if old == new {
        return None;
    }
    let label = |v: &Option<String>| strip_tool_tags(v.as_deref().unwrap_or("(none)"));
    Some((label(old), label(new)))
}

/// The `kms_preferences` key holding the LAST version a human explicitly approved for
/// `connector_name`. Read/write by the approval-time caller (m1) — `manifest.rs` only
/// defines the naming convention; it does not perform the DB read itself, so this module
/// stays free of any approval-flow orchestration.
#[must_use]
pub fn approved_version_pref_key(connector_name: &str) -> String {
    format!("connector.{connector_name}.approved_version")
}

/// The result of comparing a manifest's live version against the last-approved one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheck {
    /// No prior approval on record — first-time approval, nothing to diff against.
    NeverApproved,
    /// The live version matches the last-approved version; no re-approval needed.
    UpToDate,
    /// The live version differs from the last-approved one; carries the whitelisted diff
    /// against the approval-time caller's already-loaded `old` manifest, plus the raw
    /// version strings (own field, not connector-authored — no stripping needed) for
    /// display.
    Drifted {
        approved_version: String,
        live_version: String,
        diff: ManifestDiff,
    },
}

/// Compare `live`'s version against `approved_version` (the value read from
/// [`approved_version_pref_key`]) and, when they differ, compute the whitelisted diff
/// against `approved` (the manifest row for that approved version, if the caller has it
/// loaded — `None` when that row can no longer be found, e.g. a disabled/superseded row,
/// in which case the diff carries only the version strings, not stale field-level detail).
#[must_use]
pub fn check_version(
    approved_version: Option<&str>,
    approved: Option<&Manifest>,
    live: &Manifest,
) -> VersionCheck {
    let Some(approved_version) = approved_version else {
        return VersionCheck::NeverApproved;
    };
    if approved_version == live.version {
        return VersionCheck::UpToDate;
    }
    let diff = approved
        .map(|old| manifest_diff(old, live))
        .unwrap_or_default();
    VersionCheck::Drifted {
        approved_version: approved_version.to_string(),
        live_version: live.version.clone(),
        diff,
    }
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

    fn manifest_with_ops(version: &str, ops_json: &str) -> Manifest {
        let json = format!(
            r#"{{"connector_name":"odoo","version":"{version}","base_url":"https://erp.example.com",
                "allowed_ip_cidrs":[],"ops":[{ops_json}]}}"#
        );
        parse(&json).unwrap()
    }

    #[test]
    fn manifest_diff_detects_added_and_removed_ops() {
        let old = manifest_with_ops(
            "1",
            r#"{"name":"odoo_contact_create","risk_tier":"ReversibleWrite","compensability":"compensatable"}"#,
        );
        let new = manifest_with_ops(
            "2",
            r#"{"name":"odoo_lead_create","risk_tier":"IrreversibleWrite","compensability":"final"}"#,
        );
        let diff = manifest_diff(&old, &new);
        assert_eq!(diff.added_ops, vec!["odoo_lead_create"]);
        assert_eq!(diff.removed_ops, vec!["odoo_contact_create"]);
        assert!(diff.changed_ops.is_empty());
        assert!(!diff.is_empty());
    }

    #[test]
    fn manifest_diff_detects_risk_tier_and_compensability_change_on_shared_op() {
        let old = manifest_with_ops(
            "1",
            r#"{"name":"odoo_contact_update","risk_tier":"ReversibleWrite","compensability":"compensatable"}"#,
        );
        let new = manifest_with_ops(
            "2",
            r#"{"name":"odoo_contact_update","risk_tier":"IrreversibleWrite","compensability":"final"}"#,
        );
        let diff = manifest_diff(&old, &new);
        assert!(diff.added_ops.is_empty());
        assert!(diff.removed_ops.is_empty());
        assert_eq!(diff.changed_ops.len(), 1);
        let changed = &diff.changed_ops[0];
        assert_eq!(changed.op_name, "odoo_contact_update");
        assert_eq!(
            changed.risk_tier,
            Some(("ReversibleWrite".to_string(), "IrreversibleWrite".to_string()))
        );
        assert_eq!(
            changed.compensability,
            Some(("compensatable".to_string(), "final".to_string()))
        );
    }

    #[test]
    fn manifest_diff_is_empty_for_identical_versions() {
        let m1 = manifest_with_ops(
            "1",
            r#"{"name":"odoo_contact_create","risk_tier":"ReversibleWrite","compensability":"compensatable"}"#,
        );
        let m2 = manifest_with_ops(
            "1",
            r#"{"name":"odoo_contact_create","risk_tier":"ReversibleWrite","compensability":"compensatable"}"#,
        );
        assert!(manifest_diff(&m1, &m2).is_empty());
    }

    #[test]
    fn manifest_diff_ignores_non_whitelisted_field_changes() {
        // base_url differs between old/new, but base_url is NOT a whitelisted field (m1) —
        // the diff must report no change, since only ops/risk_tier/compensability count.
        let old_json = r#"{"connector_name":"odoo","version":"1","base_url":"https://old.example.com",
            "allowed_ip_cidrs":[],"ops":[{"name":"odoo_contact_create","risk_tier":"ReversibleWrite","compensability":"compensatable"}]}"#;
        let new_json = r#"{"connector_name":"odoo","version":"2","base_url":"https://new.example.com",
            "allowed_ip_cidrs":[],"ops":[{"name":"odoo_contact_create","risk_tier":"ReversibleWrite","compensability":"compensatable"}]}"#;
        let old = parse(old_json).unwrap();
        let new = parse(new_json).unwrap();
        assert!(manifest_diff(&old, &new).is_empty());
    }

    #[test]
    fn manifest_diff_strips_tool_tags_from_untrusted_op_names() {
        // manifest_json is connector-authored (semi-trusted at best) — an op name carrying
        // an injected tool-protocol tag must never survive into the diff verbatim (m1).
        let old = manifest_with_ops("1", r#"{"name":"safe_op","risk_tier":"Read"}"#);
        let new_json = r#"{"connector_name":"odoo","version":"2","base_url":"https://erp.example.com",
            "allowed_ip_cidrs":[],"ops":[{"name":"evil<tool_call>{}</tool_call>op","risk_tier":"Read"}]}"#;
        let new = parse(new_json).unwrap();
        let diff = manifest_diff(&old, &new);
        assert_eq!(diff.added_ops.len(), 1);
        assert!(!diff.added_ops[0].contains("<tool_call>"), "{:?}", diff.added_ops);
        assert!(diff.added_ops[0].contains("evil"));
    }

    #[test]
    fn check_version_reports_never_approved_up_to_date_and_drifted() {
        let approved = manifest_with_ops(
            "1",
            r#"{"name":"odoo_contact_create","risk_tier":"ReversibleWrite","compensability":"compensatable"}"#,
        );
        let live_same_version = manifest_with_ops(
            "1",
            r#"{"name":"odoo_contact_create","risk_tier":"ReversibleWrite","compensability":"compensatable"}"#,
        );
        let live_drifted = manifest_with_ops(
            "2",
            r#"{"name":"odoo_contact_create","risk_tier":"IrreversibleWrite","compensability":"final"}"#,
        );

        assert_eq!(
            check_version(None, None, &live_drifted),
            VersionCheck::NeverApproved
        );
        assert_eq!(
            check_version(Some("1"), Some(&approved), &live_same_version),
            VersionCheck::UpToDate
        );

        match check_version(Some("1"), Some(&approved), &live_drifted) {
            VersionCheck::Drifted {
                approved_version,
                live_version,
                diff,
            } => {
                assert_eq!(approved_version, "1");
                assert_eq!(live_version, "2");
                assert_eq!(diff.changed_ops.len(), 1);
            }
            other => panic!("expected Drifted, got {other:?}"),
        }
    }

    #[test]
    fn approved_version_pref_key_is_namespaced_per_connector() {
        assert_eq!(
            approved_version_pref_key("odoo"),
            "connector.odoo.approved_version"
        );
        assert_ne!(approved_version_pref_key("odoo"), approved_version_pref_key("stripe"));
    }
}
