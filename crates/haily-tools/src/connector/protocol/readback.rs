//! Read-back domain/kwargs construction, ported 1:1 from `OdooExecutor::read_back`'s locator
//! chain: `id_hint` FIRST (the strongest locator, and the only option for a model with no
//! correlation field, or an update whose ref was never embedded), else the model's
//! correlation field, else empty (record not locatable → caller marks `unverified`).
use crate::connector::manifest::ReadbackSpec;
use serde_json::{json, Value};

/// Build the `search_read` domain per the id-hint / correlation-field / empty priority chain.
/// `locate_by` on [`ReadbackSpec`] is documentation of INTENT, not consulted here — the
/// priority order is unconditional (matches `OdooExecutor`'s own `match` exactly, which never
/// gated on a separate preference field either).
#[must_use]
pub fn build_domain(id_hint: Option<&str>, corr_field: Option<&str>, correlation_ref: &str) -> Value {
    match (id_hint, corr_field, correlation_ref.is_empty()) {
        (Some(id), _, _) => {
            let id_num = id.parse::<i64>().map(Value::from).unwrap_or(Value::Null);
            json!([[["id", "=", id_num]]])
        }
        (None, Some(field), false) => json!([[[field, "=", correlation_ref]]]),
        _ => json!([]),
    }
}

/// The `kwargs` object for a read-back call: `limit: 1` (read-back wants exactly one record)
/// plus `protocol.context` (locale), with an `active_test: false` entry ADDED to that context
/// ONLY when `readback.active_test` is declared `true` (Odoo-shaped; harmless/ignored by a
/// connector with no such concept).
#[must_use]
pub fn build_kwargs(context: Option<&Value>, readback: Option<&ReadbackSpec>) -> Value {
    let mut ctx_obj = context.cloned().unwrap_or_else(|| json!({}));
    if readback.and_then(|r| r.active_test).unwrap_or(false) {
        if let Some(obj) = ctx_obj.as_object_mut() {
            obj.insert("active_test".to_string(), Value::Bool(false));
        }
    }
    json!({ "limit": 1, "context": ctx_obj })
}

/// Unwrap a `search_read`-shaped response per `readback.unwrap_first`: a one-element array
/// response becomes its sole element (Odoo shape); an empty array is left empty (record not
/// found); any non-array or when `unwrap_first` is unset/false is returned as-is.
#[must_use]
pub fn unwrap_first(result: Value, readback: Option<&ReadbackSpec>) -> Value {
    if readback.map(|r| r.unwrap_first).unwrap_or(false) {
        if let Value::Array(mut arr) = result {
            return if arr.is_empty() { Value::Array(arr) } else { arr.remove(0) };
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_prefers_id_hint_over_correlation_field() {
        assert_eq!(build_domain(Some("42"), Some("ref"), "corr-1"), json!([[["id", "=", 42]]]));
    }

    #[test]
    fn domain_falls_back_to_correlation_field_when_no_id_hint() {
        assert_eq!(build_domain(None, Some("ref"), "corr-1"), json!([[["ref", "=", "corr-1"]]]));
    }

    #[test]
    fn domain_is_empty_with_neither_locator() {
        assert_eq!(build_domain(None, None, ""), json!([]));
        assert_eq!(build_domain(None, Some("ref"), ""), json!([]), "empty correlation_ref never searches");
    }

    #[test]
    fn kwargs_includes_active_test_only_when_declared() {
        let ctx = json!({"lang": "en_US", "tz": "UTC"});
        let spec_on = ReadbackSpec { locate_by: None, active_test: Some(true), unwrap_first: true };
        let spec_off = ReadbackSpec { locate_by: None, active_test: Some(false), unwrap_first: true };
        let with = build_kwargs(Some(&ctx), Some(&spec_on));
        assert_eq!(with["context"]["active_test"], json!(false));
        assert_eq!(with["limit"], json!(1));
        let without = build_kwargs(Some(&ctx), Some(&spec_off));
        assert!(without["context"].get("active_test").is_none());
        let no_spec = build_kwargs(Some(&ctx), None);
        assert!(no_spec["context"].get("active_test").is_none());
    }

    #[test]
    fn unwrap_first_pulls_the_sole_element_and_leaves_empty_empty() {
        let spec = ReadbackSpec { locate_by: None, active_test: None, unwrap_first: true };
        assert_eq!(unwrap_first(json!([{"id": 1}]), Some(&spec)), json!({"id": 1}));
        assert_eq!(unwrap_first(json!([]), Some(&spec)), json!([]));
        // unwrap_first false/unset → passthrough.
        assert_eq!(unwrap_first(json!([{"id": 1}]), None), json!([{"id": 1}]));
    }
}
