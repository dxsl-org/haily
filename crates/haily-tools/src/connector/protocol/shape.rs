//! Per-method argument shaping driven by `protocol.methods` (declarative substitution), with
//! a verbatim-`args` pass-through fallback for any method the manifest does not shape —
//! mirrors `OdooExecutor::call`'s method match (`create`→`[vals]`, `write`→`[ids, values]`,
//! `unlink`/`read`→`[ids]`, else pass-through the caller's own `args`).
use crate::connector::manifest::MethodShape;
use anyhow::Result;
use serde_json::{Map, Value};

/// Shape the positional `args` for `method` from the first `protocol.methods` entry whose
/// `method` matches, substituting `ctx` into its `arg_template`. No matching entry → the
/// `fallback` (the caller's own `args`, generic pass-through) is returned un-templated.
///
/// # Errors
/// Propagates [`super::substitute`]'s fail-closed error on an unresolvable token in the
/// matched `arg_template`.
pub fn shape_args(
    methods: &[MethodShape],
    method: &str,
    ctx: &Map<String, Value>,
    fallback: Value,
) -> Result<Value> {
    match methods.iter().find(|m| m.method == method) {
        Some(shape) => super::substitute(&shape.arg_template, ctx),
        None => Ok(fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::manifest::MethodShape;
    use serde_json::json;

    fn ctx(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    fn methods() -> Vec<MethodShape> {
        vec![
            MethodShape { method: "create".to_string(), arg_template: json!(["{{values}}"]) },
            MethodShape { method: "write".to_string(), arg_template: json!(["{{ids}}", "{{values}}"]) },
            MethodShape { method: "unlink".to_string(), arg_template: json!(["{{ids}}"]) },
        ]
    }

    #[test]
    fn create_shapes_a_single_values_positional() {
        let ctx = ctx(&[("values", json!({"name": "Alice", "ref": "corr-1"}))]);
        let args = shape_args(&methods(), "create", &ctx, json!([])).unwrap();
        assert_eq!(args, json!([{"name": "Alice", "ref": "corr-1"}]));
    }

    #[test]
    fn write_shapes_ids_then_values() {
        let ctx = ctx(&[("ids", json!([7])), ("values", json!({"function": "after"}))]);
        let args = shape_args(&methods(), "write", &ctx, json!([])).unwrap();
        assert_eq!(args, json!([[7], {"function": "after"}]));
    }

    #[test]
    fn unlink_shapes_ids_only() {
        let ctx = ctx(&[("ids", json!([9]))]);
        let args = shape_args(&methods(), "unlink", &ctx, json!([])).unwrap();
        assert_eq!(args, json!([[9]]));
    }

    #[test]
    fn unmapped_method_falls_through_to_the_verbatim_fallback() {
        let ctx = ctx(&[]);
        let fallback = json!([[["model", "=", "res.partner"]], ["id"]]);
        let args = shape_args(&methods(), "search_read", &ctx, fallback.clone()).unwrap();
        assert_eq!(args, fallback);
    }
}
