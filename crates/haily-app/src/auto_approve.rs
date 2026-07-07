//! `auto_approve` allowlist — loads the config preference and validates it against
//! the tool registry at startup. A tool whose `risk_tier` CAN return
//! `RiskTier::IrreversibleWrite` for ANY args can never be listed: attempting to do
//! so is a config error at boot, not a silently-ignored entry. There is deliberately
//! no global "auto" mode — only this explicit, validated per-tool allowlist (see
//! phase-04 red team notes: a global switch is a foot-gun).
//!
//! Validation probes `risk_tier` with two synthetic argument shapes rather than
//! calling `execute` (never runs real side effects during startup validation):
//! an empty object (`json!({})`) and, if the schema declares any `required` fields,
//! a second probe that includes every required field but with a TYPE-VIOLATING value.
//! The two shapes catch different author mistakes: the empty probe catches a tool
//! that fails closed on missing args; the malformed probe catches a tool whose tier
//! depends on a required field being *present* (empty omits it) or on *parsing* that
//! field and failing closed on garbage (empty never supplies it). For today's v1
//! tools (constant-tier — no arg-branching, see `RiskTier` docs) all probes agree,
//! which is what makes `no_v1_tool_tier_varies_by_args` a soundness proof rather than
//! an assumption: the empty-object probe alone would already be sufficient, and the
//! malformed probe is belt-and-suspenders for whenever a tool branches tier on args.
use anyhow::{bail, Result};
use haily_db::queries::meta;
use haily_kms::KmsHandle;
use haily_tools::{RiskTier, ToolRegistry};
use std::collections::HashSet;

/// Build a probe that includes every schema-`required` field but with a value of
/// the WRONG type, so a tool whose tier depends on a required field — whether by
/// gating on the field's *presence* or by *parsing* it and failing closed on
/// malformed input — is forced to reveal `IrreversibleWrite` here. This is the
/// fail-closed net the empty-object probe cannot provide: the empty probe omits the
/// field entirely, so a presence-gated or parse-on-value tool slips through as "safe".
///
/// Returns `None` when the schema declares no required fields (the empty probe
/// already covers such a tool), or when no required entry is a usable field name.
fn malformed_required_field_probe(schema: &serde_json::Value) -> Option<serde_json::Value> {
    let required = schema.get("required")?.as_array()?;
    if required.is_empty() {
        return None;
    }
    let props = schema.get("properties");
    let mut probe = serde_json::Map::new();
    for field in required {
        let Some(field_name) = field.as_str() else {
            continue;
        };
        // Insert a value whose type contradicts the declared schema type, so a tool
        // that parses this field sees malformed input (→ fail-closed IrreversibleWrite)
        // AND a tool that only gates on presence sees the field present.
        let declared_type = props
            .and_then(|p| p.get(field_name))
            .and_then(|f| f.get("type"))
            .and_then(|t| t.as_str());
        let wrong_value = match declared_type {
            Some("string") => serde_json::json!(0),
            _ => serde_json::json!("__type_violation__"),
        };
        probe.insert(field_name.to_string(), wrong_value);
    }
    if probe.is_empty() {
        return None;
    }
    Some(serde_json::Value::Object(probe))
}

/// Whether `tool`'s `risk_tier` CAN return `IrreversibleWrite` for any args, probed
/// via the empty-object shape and (if applicable) the missing-required-field shape.
fn can_return_irreversible(tool: &std::sync::Arc<dyn haily_tools::Tool>) -> bool {
    let empty_probe = serde_json::json!({});
    if tool.risk_tier(&empty_probe) == RiskTier::IrreversibleWrite {
        return true;
    }
    if let Some(probe) = malformed_required_field_probe(&tool.parameters_schema()) {
        if tool.risk_tier(&probe) == RiskTier::IrreversibleWrite {
            return true;
        }
    }
    false
}

/// KMS preference key: JSON array of tool names, e.g. `["task_delete"]`.
const AUTO_APPROVE_PREF_KEY: &str = "approval.auto_approve";

/// Load the raw `auto_approve` list from KMS preferences (empty if unset or
/// malformed — a malformed preference must not crash startup, but IS still subject
/// to `validate` below once parsed, so partial/garbage JSON just yields "no
/// auto-approvals" rather than a config error).
pub async fn load_auto_approve(kms: &KmsHandle) -> Vec<String> {
    let db = kms.db();
    match meta::get_preference(db, AUTO_APPROVE_PREF_KEY).await {
        Ok(Some(json)) => serde_json::from_str::<Vec<String>>(&json).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Validate `names` against `registry`: any name resolving to a tool whose
/// `risk_tier` CAN return `IrreversibleWrite` for ANY probed args is a startup
/// config error — those tools (worktree apply, the delete tools, `http_request`)
/// must always ask, with no bypass. Unknown tool names are also rejected (silently
/// accepting a typo would hide a no-op config entry from the operator).
///
/// Returns the validated set on success, ready to hand to
/// `ApprovalBroker::with_auto_approve`.
pub fn validate_auto_approve(names: &[String], registry: &ToolRegistry) -> Result<HashSet<String>> {
    let mut validated = HashSet::with_capacity(names.len());
    for name in names {
        let Some(tool) = registry.get(name) else {
            bail!("auto_approve config error: unknown tool '{name}'");
        };
        if can_return_irreversible(tool) {
            bail!(
                "auto_approve config error: '{name}' can require user approval \
                 (risk_tier resolves to IrreversibleWrite for some args) and can never be \
                 auto-approved (destructive/exfiltrating tools are always-ask)"
            );
        }
        validated.insert(name.clone());
    }
    Ok(validated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_require_approval_tool() {
        let registry = ToolRegistry::build_v1();
        let names = vec!["worktree_apply".to_string()];
        let err = validate_auto_approve(&names, &registry).unwrap_err();
        assert!(err.to_string().contains("worktree_apply"));
    }

    #[test]
    fn rejects_every_tool_that_can_still_require_approval() {
        // These remain `IrreversibleWrite`-capable (never re-tiered) — always-ask, no
        // auto-approve bypass permitted for any of them. `memory_forget` moved OFF this
        // list in Phase 12 (memory-undo via KmsHandle compensator); `calendar_delete`
        // moved off in Phase 13b (assistant-depth: occurrence-vs-series undo +
        // exceptions) — see `retiered_delete_tools_are_no_longer_rejected` below.
        let registry = ToolRegistry::build_v1();
        for name in ["http_request"] {
            let names = vec![name.to_string()];
            assert!(
                validate_auto_approve(&names, &registry).is_err(),
                "expected '{name}' to be rejected as a startup config error"
            );
        }
    }

    /// Harness Completion phase 2 (`task_delete`/`note_delete`/`reminder_delete`),
    /// Phase 12 (`memory_forget`, memory-undo via KmsHandle compensator), and Phase 13b
    /// assistant-depth (`calendar_delete`, occurrence-vs-series undo + exceptions) all
    /// re-tier to CONSTANT `ReversibleWrite` (journaled + undoable) —
    /// `can_return_irreversible` correctly reports `false` for them, so
    /// `validate_auto_approve` no longer rejects naming them (doing so is a harmless
    /// no-op: they never gate on approval at all now, re-tiering already removed the
    /// prompt these tools used to be gated behind).
    #[test]
    fn retiered_delete_tools_are_no_longer_rejected() {
        let registry = ToolRegistry::build_v1();
        for name in [
            "task_delete",
            "reminder_delete",
            "note_delete",
            "memory_forget",
            "calendar_delete",
        ] {
            let names = vec![name.to_string()];
            let result = validate_auto_approve(&names, &registry);
            assert!(
                result.is_ok(),
                "'{name}' is re-tiered ReversibleWrite and never returns IrreversibleWrite \
                 — it must not be rejected: {result:?}"
            );
        }
    }

    #[test]
    fn rejects_unknown_tool_name() {
        let registry = ToolRegistry::build_v1();
        let names = vec!["does_not_exist".to_string()];
        assert!(validate_auto_approve(&names, &registry).is_err());
    }

    #[test]
    fn accepts_an_auto_approve_class_tool() {
        let registry = ToolRegistry::build_v1();
        let names = vec!["web_search".to_string()];
        let validated = validate_auto_approve(&names, &registry)
            .expect("AutoApprove-class tool should be accepted");
        assert!(validated.contains("web_search"));
    }

    #[test]
    fn empty_list_is_always_valid() {
        let registry = ToolRegistry::build_v1();
        let validated =
            validate_auto_approve(&[], &registry).expect("empty allowlist is the default");
        assert!(validated.is_empty());
    }

    /// M9 (phase-1 success criteria): reject any tool whose `risk_tier` CAN return
    /// `IrreversibleWrite` for ANY probed args — this is the general form of
    /// `rejects_every_delete_and_http_tool` above, phrased as "every v1 tool that
    /// resolves IrreversibleWrite on the empty probe is rejected", proving the
    /// validator's decision rule directly rather than an enumerated tool list.
    #[test]
    fn rejects_any_tool_that_can_return_irreversible() {
        let registry = ToolRegistry::build_v1();
        for tool in registry.list() {
            let name = tool.name().to_string();
            let expect_rejected =
                tool.risk_tier(&serde_json::json!({})) == RiskTier::IrreversibleWrite;
            let result = validate_auto_approve(std::slice::from_ref(&name), &registry);
            assert_eq!(
                result.is_err(),
                expect_rejected,
                "'{name}': validate_auto_approve err={:?} but risk_tier(empty)={:?}",
                result.is_err(),
                tool.risk_tier(&serde_json::json!({}))
            );
        }
    }

    /// M9 fail-closed probe (registry side): any v1 tool that resolves
    /// `IrreversibleWrite` under the malformed-required-field probe must be rejected
    /// by the validator, not just under the empty-object probe.
    #[test]
    fn rejects_on_malformed_required_field_probe() {
        let registry = ToolRegistry::build_v1();
        for tool in registry.list() {
            let schema = tool.parameters_schema();
            let Some(probe) = malformed_required_field_probe(&schema) else {
                continue; // tool has no required fields — empty probe already covers it
            };
            if tool.risk_tier(&probe) == RiskTier::IrreversibleWrite {
                let name = tool.name().to_string();
                assert!(
                    validate_auto_approve(std::slice::from_ref(&name), &registry).is_err(),
                    "'{name}': risk_tier(malformed_required)=IrreversibleWrite but validator accepted it"
                );
            }
        }
    }

    /// M9 fail-closed probe (helper side — the non-vacuous proof the guard is real):
    /// for a schema with required fields the probe MUST be a non-empty object that
    /// includes each required field with a type-violating value — i.e. distinct from
    /// the empty-object probe. This is what a presence-gated or parse-on-value tool
    /// would be forced to reveal itself against; a probe of `json!({})` (the prior
    /// bug) would silently be a duplicate of the empty probe and catch nothing.
    #[test]
    fn malformed_probe_populates_required_fields_with_wrong_types() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "target_id": { "type": "string" }, "count": { "type": "number" } },
            "required": ["target_id", "count"]
        });
        let probe = malformed_required_field_probe(&schema).expect("required fields ⇒ Some probe");
        assert_ne!(
            probe,
            serde_json::json!({}),
            "probe must not duplicate the empty-object probe"
        );
        assert!(
            probe.get("target_id").is_some() && probe.get("count").is_some(),
            "probe must include every required field"
        );
        assert!(
            !probe["target_id"].is_string(),
            "required string field must be type-violated"
        );
        assert!(
            !probe["count"].is_number(),
            "required number field must be type-violated"
        );

        // No required fields ⇒ None (empty probe already covers the tool).
        let no_req =
            serde_json::json!({ "type": "object", "properties": { "x": { "type": "string" } } });
        assert!(malformed_required_field_probe(&no_req).is_none());
        // Empty `required` array ⇒ None.
        let empty_req = serde_json::json!({ "type": "object", "required": [] });
        assert!(malformed_required_field_probe(&empty_req).is_none());
    }

    /// Phase-1 success criteria: every `build_v1` tool returns the SAME tier for
    /// `json!({})` and a populated args object — proving the empty-probe validation
    /// above is sound for v1 (no tool branches its tier on args yet). The moment a
    /// tool starts arg-branching its tier, this test's assumption breaks and the
    /// M9 validator must be revisited (see `RiskTier` module docs).
    #[test]
    fn no_v1_tool_tier_varies_by_args() {
        let registry = ToolRegistry::build_v1();
        let populated = serde_json::json!({
            "id": "some-id",
            "title": "some title",
            "content": "some content",
            "query": "some query",
            "url": "https://example.com",
            "method": "POST",
            "task": "some task",
            "subject": "s", "predicate": "p", "object": "o",
            "start_at": "2026-01-01T00:00:00Z", "end_at": "2026-01-01T01:00:00Z",
            "fire_at": "2026-01-01T00:00:00Z",
            "worktree_path": "/tmp/example",
            "reaction": "positive",
            "filter": "all",
        });
        for tool in registry.list() {
            let empty_tier = tool.risk_tier(&serde_json::json!({}));
            let populated_tier = tool.risk_tier(&populated);
            assert_eq!(
                empty_tier,
                populated_tier,
                "'{}': tier varies by args (empty={:?}, populated={:?}) — M9 empty-probe validation is no longer sound",
                tool.name(),
                empty_tier,
                populated_tier
            );
        }
    }
}
