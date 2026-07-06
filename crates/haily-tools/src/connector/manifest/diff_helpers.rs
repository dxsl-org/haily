//! Low-level field-diff primitives for [`super::manifest_diff`], split out so the
//! orchestration in `diff.rs` (what gets compared) stays separate from how each field shape
//! is rendered/compared.
use super::ManifestDiff;
use crate::connector::manifest::schema::{AuthSpec, MethodShape, ProtocolSpec};
use crate::connector::redact::strip_tool_tags;
use serde::Serialize;

/// Extract `(scheme, cred_ref, header_name, param_name)`, all `None` when `auth` is absent.
/// Destructuring `AuthSpec` by name (rather than `..`) means adding a new field to the
/// struct fails this match until the new field is threaded through the diff too — the
/// compile-time guard against a silent re-approval bypass (see phase risk notes).
pub(super) fn auth_fields(
    auth: &Option<AuthSpec>,
) -> (Option<String>, Option<String>, Option<String>, Option<String>) {
    match auth.as_ref() {
        Some(AuthSpec {
            scheme,
            cred_ref,
            header_name,
            param_name,
        }) => (
            Some(scheme.clone()),
            Some(cred_ref.clone()),
            header_name.clone(),
            param_name.clone(),
        ),
        None => (None, None, None, None),
    }
}

/// Diff every `ProtocolSpec` sub-field. Missing `protocol` on either side is treated as
/// `ProtocolSpec::default()` (all-empty) so e.g. adding a `protocol` section with one
/// `fault_rules` entry surfaces as a `protocol_fault_rules` change, not a no-op. Exhaustive
/// field-by-field destructuring is the same compile-time completeness guard as
/// [`auth_fields`] — a new `ProtocolSpec` field will not compile here until handled.
pub(super) fn diff_protocol(
    old: &Option<ProtocolSpec>,
    new: &Option<ProtocolSpec>,
    diff: &mut ManifestDiff,
) {
    let ProtocolSpec {
        endpoint_suffix: oe,
        envelope: oenv,
        methods: om,
        fault_rules: ofr,
        readback: orb,
        context: octx,
        prevalidate: opv,
    } = old.clone().unwrap_or_default();
    let ProtocolSpec {
        endpoint_suffix: ne,
        envelope: nenv,
        methods: nm,
        fault_rules: nfr,
        readback: nrb,
        context: nctx,
        prevalidate: npv,
    } = new.clone().unwrap_or_default();

    diff.protocol_endpoint_suffix = diff_field(&oe, &ne);
    diff.protocol_envelope = diff_serialized(&oenv, &nenv);
    diff.protocol_methods = diff_methods(&om, &nm);
    diff.protocol_fault_rules = diff_serialized(&ofr, &nfr);
    diff.protocol_readback = diff_serialized(&orb, &nrb);
    diff.protocol_context = diff_serialized(&octx, &nctx);
    diff.protocol_prevalidate = diff_serialized(&opv, &npv);
}

/// Render a `Vec<MethodShape>` change, tag-stripping each `method` name before it enters
/// the JSON-string representation (the shared [`diff_serialized`] path only tag-strips at
/// the top level of primitive strings, not inside nested `Serialize` structures).
fn diff_methods(old: &[MethodShape], new: &[MethodShape]) -> Option<(String, String)> {
    if old == new {
        return None;
    }
    let sanitize = |m: &MethodShape| MethodShape {
        method: strip_tool_tags(&m.method),
        arg_template: m.arg_template.clone(),
    };
    let render = |methods: &[MethodShape]| {
        let sanitized: Vec<MethodShape> = methods.iter().map(sanitize).collect();
        serde_json::to_string(&sanitized).unwrap_or_default()
    };
    Some((render(old), render(new)))
}

/// `Some((old, new))` JSON-string rendering when two `Serialize` values differ. Used for
/// the protocol sub-fields that carry `serde_json::Value` or a struct shape rather than a
/// bare string. NOTE: this does NOT tag-strip nested string content — callers holding
/// connector-authored free text (e.g. `methods[].method`) use a dedicated sanitizing
/// renderer instead (see [`diff_methods`]); the fields diffed here (`envelope`, `context`,
/// `fault_rules`, `readback`, `prevalidate`) are either operator-authored templates/match
/// tables (not third-party record data) or have no request-response round trip that could
/// inject a tag through this path at diff time.
fn diff_serialized<T: Serialize + PartialEq>(old: &T, new: &T) -> Option<(String, String)> {
    if old == new {
        return None;
    }
    let render = |v: &T| serde_json::to_string(v).unwrap_or_default();
    Some((render(old), render(new)))
}

/// `Some((old, new))`, tag-stripped, when the two optional string fields differ (treating
/// absent as the literal `"(none)"` label so a field that newly appears/disappears is
/// still surfaced instead of silently comparing `None == None`).
pub(super) fn diff_field(old: &Option<String>, new: &Option<String>) -> Option<(String, String)> {
    if old == new {
        return None;
    }
    let label = |v: &Option<String>| strip_tool_tags(v.as_deref().unwrap_or("(none)"));
    Some((label(old), label(new)))
}

/// Same as [`diff_field`] but for required (non-`Option`) string fields, e.g. `base_url`.
pub(super) fn diff_field_str(old: &str, new: &str) -> Option<(String, String)> {
    if old == new {
        return None;
    }
    Some((strip_tool_tags(old), strip_tool_tags(new)))
}

/// Order-insensitive CIDR list diff (M1) — a reorder of the same set is not a change, only
/// an actual addition/removal/substitution is.
pub(super) fn diff_cidrs(old: &[String], new: &[String]) -> Option<(Vec<String>, Vec<String>)> {
    let mut old_sorted: Vec<String> = old.iter().map(|s| strip_tool_tags(s)).collect();
    let mut new_sorted: Vec<String> = new.iter().map(|s| strip_tool_tags(s)).collect();
    old_sorted.sort();
    new_sorted.sort();
    if old_sorted == new_sorted {
        return None;
    }
    Some((old_sorted, new_sorted))
}
