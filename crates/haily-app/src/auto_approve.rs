//! `auto_approve` allowlist — loads the config preference and validates it against
//! the tool registry at startup. Destructive/exfil-class tools (anything the
//! registry marks `ToolClass::RequireApproval`) can never be listed: attempting to
//! do so is a config error at boot, not a silently-ignored entry. There is
//! deliberately no global "auto" mode — only this explicit, validated per-tool
//! allowlist (see phase-04 red team notes: a global switch is a foot-gun).
use anyhow::{bail, Result};
use haily_db::queries::meta;
use haily_kms::KmsHandle;
use haily_tools::{ToolClass, ToolRegistry};
use std::collections::HashSet;

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
/// `approval_class()` is `RequireApproval` is a startup config error — those tools
/// (worktree apply, the delete tools, `http_request`) must always ask, with no
/// bypass. Unknown tool names are also rejected (silently accepting a typo would
/// hide a no-op config entry from the operator).
///
/// Returns the validated set on success, ready to hand to
/// `ApprovalBroker::with_auto_approve`.
pub fn validate_auto_approve(names: &[String], registry: &ToolRegistry) -> Result<HashSet<String>> {
    let mut validated = HashSet::with_capacity(names.len());
    for name in names {
        let Some(tool) = registry.get(name) else {
            bail!("auto_approve config error: unknown tool '{name}'");
        };
        if tool.approval_class() == ToolClass::RequireApproval {
            bail!(
                "auto_approve config error: '{name}' requires user approval and can never be \
                 auto-approved (destructive/exfiltrating tool classes are always-ask)"
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
    fn rejects_every_delete_and_http_tool() {
        let registry = ToolRegistry::build_v1();
        for name in ["task_delete", "reminder_delete", "note_delete", "memory_forget", "calendar_delete", "http_request"] {
            let names = vec![name.to_string()];
            assert!(
                validate_auto_approve(&names, &registry).is_err(),
                "expected '{name}' to be rejected as a startup config error"
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
        let validated = validate_auto_approve(&names, &registry).expect("AutoApprove-class tool should be accepted");
        assert!(validated.contains("web_search"));
    }

    #[test]
    fn empty_list_is_always_valid() {
        let registry = ToolRegistry::build_v1();
        let validated = validate_auto_approve(&[], &registry).expect("empty allowlist is the default");
        assert!(validated.is_empty());
    }
}
