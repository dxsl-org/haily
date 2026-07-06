//! `(model, method)` resolution + correlation-field lookup + client-side prevalidation —
//! ported 1:1 from `OdooExecutor`'s private helpers (odoo_executor.rs `op_model_method`,
//! `prevalidate`, `correlation_field_for`) so the generic interpreter fails closed EXACTLY
//! the same way: a manifest op with no declared model/method, or a compensation op whose plan
//! carries no model, is an error — never a guessed model (M5b parity target).
use crate::connector::manifest::{Manifest, ModelRequiredFields};
use anyhow::{Context, Result};
use serde_json::Value;

/// Resolve `(model, method)` for `op`. `op` is EITHER a manifest op NAME (the primary write)
/// or a bare compensation-op keyword (`write`/`unlink`/...) the undo logic passes from the
/// plan — for the latter, model/method travel on `params` (the compensation plan enriched
/// with `model` at manifest-approval time), since a compensation op is not itself a manifest
/// op name.
///
/// # Errors
/// Fail-closed: a manifest op with no declared model/method, or a compensation op with no
/// model on its plan, is an error — never a guessed model.
pub fn resolve_op_model_method(manifest: &Manifest, op: &str, params: &Value) -> Result<(String, String)> {
    if let Some(spec) = manifest.ops.iter().find(|o| o.name == op) {
        let model = spec.model.clone().with_context(|| format!("op '{op}' has no model"))?;
        let method = spec.method.clone().with_context(|| format!("op '{op}' has no method"))?;
        return Ok((model, method));
    }
    let model = params
        .get("model")
        .and_then(Value::as_str)
        .with_context(|| format!("compensation op '{op}' has no model on its plan"))?
        .to_string();
    // `method` defaults from the compensation keyword: archive/write → write, unlink → unlink,
    // read → search_read — matching `OdooExecutor::op_model_method`'s default chain.
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

/// The model's correlation field name, resolved from the FIRST manifest op declaring `model`
/// with a `correlation_field`. `None` when the model has no such field (e.g. `mail.activity`)
/// — the caller then neither writes nor searches by a ref for it.
#[must_use]
pub fn correlation_field_for(manifest: &Manifest, model: &str) -> Option<String> {
    manifest
        .ops
        .iter()
        .filter(|o| o.model.as_deref() == Some(model))
        .find_map(|o| o.correlation_field.clone())
}

/// Client-side required-field check driven by `protocol.prevalidate`, run before any network
/// call for a `create`. A missing/empty required field is a caller error — nothing was sent.
///
/// # Errors
/// Returns an error naming the first missing required field for `model`.
pub fn prevalidate(rules: &[ModelRequiredFields], model: &str, values: &Value) -> Result<()> {
    for rule in rules.iter().filter(|r| r.model == model) {
        for field in &rule.required_fields {
            let present = values
                .get(field)
                .map(|v| !v.is_null() && v != &Value::String(String::new()))
                .unwrap_or(false);
            if !present {
                anyhow::bail!("{model} requires '{field}' for create");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::manifest;
    use serde_json::json;

    fn manifest_with_op() -> Manifest {
        manifest::parse(
            r#"{"connector_name":"odoo","version":"1","base_url":"https://erp.example.com",
                "allowed_ip_cidrs":[],
                "ops":[{"name":"odoo_contact_create","model":"res.partner","method":"create",
                        "risk_tier":"ReversibleWrite","correlation_field":"ref",
                        "compensability":"compensatable",
                        "compensation":{"op":"archive","model":"res.partner","method":"write"}}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn resolves_manifest_op_name_and_compensation_plan_op() {
        let m = manifest_with_op();
        let (model, method) = resolve_op_model_method(&m, "odoo_contact_create", &Value::Null).unwrap();
        assert_eq!((model.as_str(), method.as_str()), ("res.partner", "create"));

        // A bare compensation keyword resolves from the PLAN, not the manifest.
        let plan = json!({"op": "write", "model": "res.partner", "method": "write", "ids": [7]});
        let (cm, cmethod) = resolve_op_model_method(&m, "write", &plan).unwrap();
        assert_eq!((cm.as_str(), cmethod.as_str()), ("res.partner", "write"));

        // No model on the plan → fail closed, never a guessed model.
        assert!(resolve_op_model_method(&m, "write", &json!({"op": "write"})).is_err());

        // Method defaults by keyword when the plan omits it.
        let (_m, um) = resolve_op_model_method(&m, "unlink", &json!({"model": "mail.activity"})).unwrap();
        assert_eq!(um, "unlink");
    }

    #[test]
    fn correlation_field_resolves_first_declaring_op_or_none() {
        let m = manifest_with_op();
        assert_eq!(correlation_field_for(&m, "res.partner").as_deref(), Some("ref"));
        assert_eq!(correlation_field_for(&m, "mail.activity"), None);
    }

    #[test]
    fn prevalidate_enforces_required_fields_and_treats_empty_as_absent() {
        let rules = vec![ModelRequiredFields {
            model: "res.partner".to_string(),
            required_fields: vec!["name".to_string()],
        }];
        assert!(prevalidate(&rules, "res.partner", &json!({"name": "Alice"})).is_ok());
        assert!(prevalidate(&rules, "res.partner", &json!({"email": "a@b.c"})).is_err());
        assert!(prevalidate(&rules, "res.partner", &json!({"name": ""})).is_err());
        // A model with no declared rule has no requirements.
        assert!(prevalidate(&rules, "x.other", &json!({})).is_ok());
    }
}
