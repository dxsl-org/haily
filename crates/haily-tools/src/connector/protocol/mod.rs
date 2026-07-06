//! Declarative substitution-only templating (locked at plan level) plus the small set of
//! helpers `HttpExecutor` composes to INTERPRET `protocol.*` — everything `OdooExecutor`
//! hardcodes, expressed as manifest data instead of Rust match arms:
//! [`op_resolve`] ((model, method) resolution, correlation field, prevalidate),
//! [`shape`] (per-method arg templating), [`fault`] (fault-rule classification),
//! [`readback`] (locate-domain + kwargs + unwrap), [`overlay`] (the M4 `connection` seam).
//!
//! ## Substitution grammar
//!
//! `protocol.envelope` / `methods[].arg_template` reference `{{token}}` placeholders,
//! substituted at the [`serde_json::Value`] level — a substituted leaf BECOMES the
//! context's JSON value (string/number/array/object), never a spliced string, so a
//! placeholder can never reshape the surrounding JSON structure (no injection surface). A
//! JSON string leaf that is EXACTLY `"{{name}}"` (no surrounding text) is a placeholder;
//! any other string — including one that merely CONTAINS `{{...}}` as a substring — is a
//! literal, copied through unchanged. There is NO conditional, loop, or eval.
//!
//! The "whitelist" of resolvable tokens is enforced BY CONSTRUCTION rather than a table
//! duplicated here: only this crate's own Rust code ever inserts entries into the `ctx` map
//! passed to [`substitute`], so a manifest can reference an EXISTING token but can never
//! invent a new one. A `{{token}}` whose name is not a key in `ctx` is an ERROR — fail
//! closed, never silently emitted as a literal or guessed.
use anyhow::{Context, Result};
use serde_json::{Map, Value};

pub mod fault;
pub mod op_resolve;
pub mod overlay;
pub mod readback;
pub mod shape;

pub use overlay::ConnectionOverlay;

/// Substitute `{{token}}` placeholders in `template` using `ctx` (token name → JSON value).
/// Recurses through objects/arrays; non-placeholder scalars pass through unchanged.
///
/// # Errors
/// Returns an error when a string leaf is a well-formed `{{token}}` placeholder whose token
/// is NOT a key in `ctx` — fail-closed, never silently dropped or left as a literal.
pub fn substitute(template: &Value, ctx: &Map<String, Value>) -> Result<Value> {
    match template {
        Value::String(s) => match placeholder_token(s) {
            Some(token) => ctx.get(token).cloned().with_context(|| {
                format!("connector protocol template references unresolvable token {{{{{token}}}}}")
            }),
            None => Ok(Value::String(s.clone())),
        },
        Value::Array(items) => items
            .iter()
            .map(|v| substitute(v, ctx))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), substitute(v, ctx)?);
            }
            Ok(Value::Object(out))
        }
        scalar => Ok(scalar.clone()),
    }
}

/// `Some(name)` when `s` is EXACTLY `"{{name}}"`; `None` for anything else (a literal,
/// including a string that merely contains braces as a substring).
fn placeholder_token(s: &str) -> Option<&str> {
    s.strip_prefix("{{")?.strip_suffix("}}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn substitutes_whole_string_tokens_as_typed_values() {
        let template = json!({"a": "{{num}}", "b": ["{{arr}}", "literal"], "c": "no-token-here"});
        let c = ctx(&[("num", json!(2)), ("arr", json!([1, 2, 3]))]);
        let out = substitute(&template, &c).unwrap();
        // Typed substitution: the number token becomes a JSON number, not the string "2".
        assert_eq!(out["a"], json!(2));
        assert_eq!(out["b"][0], json!([1, 2, 3]));
        assert_eq!(out["b"][1], json!("literal"));
        assert_eq!(out["c"], json!("no-token-here"));
    }

    #[test]
    fn partial_or_embedded_braces_are_literal_not_substituted() {
        let template = json!("prefix {{model}} suffix");
        let c = ctx(&[("model", json!("res.partner"))]);
        // NOT a whole-string match → literal, untouched (no partial string interpolation).
        assert_eq!(substitute(&template, &c).unwrap(), template);
    }

    #[test]
    fn unresolvable_token_fails_closed() {
        let template = json!({"x": "{{ghost}}"});
        let c = ctx(&[("model", json!("res.partner"))]);
        assert!(substitute(&template, &c).is_err(), "unknown token must error, never guess");
    }

    #[test]
    fn nested_objects_and_scalars_pass_through() {
        let template = json!({"n": 42, "b": true, "nested": {"deep": "{{v}}"}});
        let c = ctx(&[("v", json!("ok"))]);
        let out = substitute(&template, &c).unwrap();
        assert_eq!(out["n"], json!(42));
        assert_eq!(out["b"], json!(true));
        assert_eq!(out["nested"]["deep"], json!("ok"));
    }
}
