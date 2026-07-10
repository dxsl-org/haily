//! GBNF grammar generation from tool JSON schemas — the constrained-decoding building
//! block for the in-process llama backend (research: forcing valid tool-call syntax
//! eliminates malformed JSON from weak local models).
//!
//! FEATURE-INDEPENDENT BY CONTRACT: this module has no `llama` dependency and compiles
//! and its tests run WITHOUT `--features llama`. It emits a grammar *string*; the llama
//! sampler (`llama.rs`, `#[cfg(feature = "llama")]`) is the only consumer of that string
//! and is responsible for feeding it to llama-cpp-2's grammar sampler (with an
//! unconstrained fallback if construction fails).
//!
//! SECURITY BOUNDARY (red-team SEC-MED): the grammar constrains SYNTAX only — it forces
//! well-formed JSON with a real tool name and roughly-correct argument shape. It does
//! NOT validate argument VALUES; a grammar-valid call can still carry a malicious path
//! or shell string. Those are rejected downstream by dispatch's path-guard / shell-policy
//! (P1) — the value check is deliberately NOT duplicated here (see the
//! `grammar_does_not_validate_values` test asserting this boundary).
//!
//! SCHEMA SUBSET: object / string / number|integer / boolean / null / array / enum. A
//! property (or whole tool) whose schema falls outside this subset is SKIPPED — the tool
//! contributes no alternative, and if that leaves the grammar empty the generator returns
//! `None` so the caller falls back to unconstrained generation. It NEVER panics.

use serde_json::Value;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::OnceLock;

/// The tool-call envelope the agent loop parses (`haily-core::tool_call::parse_tool_call`):
/// `<tool_call>{"tool":"<name>","args":<value>}</tool_call>`. The grammar's `root` is an
/// alternation of one such envelope per tool, each fixing its own name + args schema.
///
/// `tools` is `(name, parameters_schema)` pairs — the caller (P4, in haily-core) reads
/// these from the `ToolRegistry`; haily-llm is a leaf crate and cannot depend on
/// haily-tools, so the input is plain `serde_json`.
///
/// Returns `None` when `tools` is empty or every tool's schema is unsupported — the
/// caller then generates unconstrained (the forced-JSON contracts must survive GBNF
/// being unavailable). Result is cached per tool-set hash.
pub fn tool_call_grammar(tools: &[(&str, &Value)]) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let key = tool_set_hash(tools);
    if let Some(hit) = cache().lock().ok().and_then(|c| c.get(&key).cloned()) {
        return hit;
    }
    let generated = build_grammar(tools);
    if let Ok(mut c) = cache().lock() {
        c.insert(key, generated.clone());
    }
    generated
}

fn cache() -> &'static Mutex<HashMap<u64, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tool_set_hash(tools: &[(&str, &Value)]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (name, schema) in tools {
        name.hash(&mut hasher);
        // Value isn't Hash; its canonical serialization is a stable proxy for this cache.
        serde_json::to_string(schema).unwrap_or_default().hash(&mut hasher);
    }
    hasher.finish()
}

/// GBNF prelude: the generic JSON primitives every generated grammar can reference.
/// Unused rules are harmless (llama.cpp only walks rules reachable from `root`).
const PRELUDE: &str = r#"ws ::= [ \t\n]*
value ::= object | array | string | number | boolean | "null"
object ::= "{" ws ( member ( ws "," ws member )* )? ws "}"
member ::= string ws ":" ws value
array ::= "[" ws ( value ( ws "," ws value )* )? ws "]"
string ::= "\"" strchar* "\""
strchar ::= [^"\\] | "\\" (["\\/bfnrt] | "u" [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F])
number ::= "-"? ("0" | [1-9] [0-9]*) ("." [0-9]+)? ([eE] [-+]? [0-9]+)?
boolean ::= "true" | "false""#;

/// Accumulates named GBNF rules and mints unique names for object/array sub-schemas.
struct Grammar {
    rules: Vec<String>,
    counter: usize,
}

impl Grammar {
    fn new() -> Self {
        Self { rules: Vec::new(), counter: 0 }
    }

    fn fresh(&mut self, prefix: &str) -> String {
        let name = format!("{prefix}{}", self.counter);
        self.counter += 1;
        name
    }

    fn add(&mut self, name: &str, body: &str) {
        self.rules.push(format!("{name} ::= {body}"));
    }

    /// GBNF expression matching a JSON value conforming to `schema` (subset only).
    /// Returns `None` for an unsupported/malformed schema so the caller can skip the
    /// offending property or whole tool rather than emit a broken grammar.
    fn value_expr(&mut self, schema: &Value) -> Option<String> {
        let obj = schema.as_object()?; // a schema must be a JSON object

        // `enum` takes precedence over `type`: fix the value to one of the literals.
        if let Some(Value::Array(variants)) = obj.get("enum") {
            if variants.is_empty() {
                return None; // an empty enum matches nothing — unsupported
            }
            let alts: Vec<String> = variants
                .iter()
                .map(|v| gbnf_literal(&serde_json::to_string(v).unwrap_or_default()))
                .collect();
            return Some(format!("({})", alts.join(" | ")));
        }

        match obj.get("type").and_then(Value::as_str) {
            Some("string") => Some("string".to_string()),
            Some("integer") | Some("number") => Some("number".to_string()),
            Some("boolean") => Some("boolean".to_string()),
            Some("null") => Some("\"null\"".to_string()),
            Some("array") => self.array_expr(obj),
            Some("object") => self.object_expr(obj),
            // No `type` and no `enum`: accept any JSON value (generic).
            None => Some("value".to_string()),
            // A `type` outside the supported subset (or a non-string / union `type`).
            Some(_) => None,
        }
    }

    fn array_expr(&mut self, obj: &serde_json::Map<String, Value>) -> Option<String> {
        match obj.get("items") {
            Some(items) => {
                let item = self.value_expr(items)?;
                let name = self.fresh("arr");
                self.add(&name, &format!("\"[\" ws ( {item} ( ws \",\" ws {item} )* )? ws \"]\""));
                Some(name)
            }
            None => Some("array".to_string()), // untyped items → generic array
        }
    }

    fn object_expr(&mut self, obj: &serde_json::Map<String, Value>) -> Option<String> {
        let Some(Value::Object(props)) = obj.get("properties") else {
            return Some("object".to_string()); // no declared props → generic object
        };
        if props.is_empty() {
            return Some("object".to_string());
        }
        let required: Vec<&str> = obj
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let mut req_parts = Vec::new();
        let mut opt_parts = Vec::new();
        for (key, subschema) in props {
            let vexpr = self.value_expr(subschema)?; // any unsupported prop → skip tool
            let key_token = gbnf_literal(&serde_json::to_string(key).unwrap_or_default());
            let part = format!("{key_token} ws \":\" ws {vexpr}");
            if required.contains(&key.as_str()) {
                req_parts.push(part);
            } else {
                opt_parts.push(part);
            }
        }

        // With zero required properties, an order-independent grammar for optional-only
        // objects is combinatorial — fall back to the generic object rule (still valid
        // JSON; the value validator does the semantic check). With >=1 required prop,
        // each optional carries its own leading comma so any subset is expressible.
        if req_parts.is_empty() {
            return Some("object".to_string());
        }
        let mut body = String::from("\"{\" ws ");
        body.push_str(&req_parts.join(" ws \",\" ws "));
        for opt in opt_parts {
            body.push_str(&format!(" ( ws \",\" ws {opt} )?"));
        }
        body.push_str(" ws \"}\"");
        let name = self.fresh("obj");
        self.add(&name, &body);
        Some(name)
    }
}

fn build_grammar(tools: &[(&str, &Value)]) -> Option<String> {
    let mut g = Grammar::new();
    let mut call_rules = Vec::new();

    for (name, schema) in tools {
        // A tool whose args schema is unsupported is skipped entirely (no alternative).
        let Some(args_expr) = g.value_expr(schema) else {
            continue;
        };
        let name_token = gbnf_literal(&format!("\"{name}\"")); // JSON string token for the name
        let call = g.fresh("call");
        g.add(
            &call,
            &format!(
                "\"<tool_call>\" ws \"{{\" ws \"\\\"tool\\\"\" ws \":\" ws {name_token} ws \",\" ws \"\\\"args\\\"\" ws \":\" ws {args_expr} ws \"}}\" ws \"</tool_call>\""
            ),
        );
        call_rules.push(call);
    }

    if call_rules.is_empty() {
        return None; // every tool unsupported → caller falls back to unconstrained
    }

    let mut out = String::new();
    out.push_str(&format!("root ::= {}\n", call_rules.join(" | ")));
    out.push_str(PRELUDE);
    out.push('\n');
    for rule in &g.rules {
        out.push_str(rule);
        out.push('\n');
    }
    Some(out)
}

/// Wrap raw terminal text `s` as a GBNF double-quoted string literal, escaping the two
/// characters GBNF treats specially inside a literal (`\` and `"`). `s` is the EXACT
/// text to match — e.g. for a JSON string token `"foo"` this yields `"\"foo\""`.
fn gbnf_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Lightweight GBNF structural validator — feature-independent stand-in for the real
    /// llama.cpp parser (which needs a loaded model's vocab and the `llama` feature).
    /// Verifies: a `root` rule exists, and every referenced rule name resolves to a
    /// definition. Good enough to catch a broken generator; the actual parse is exercised
    /// (unverifiable on a CI host without a native toolchain) by the cfg-gated sampler in
    /// `llama.rs`.
    fn assert_parseable(grammar: &str) {
        let mut defined = std::collections::HashSet::new();
        for line in grammar.lines() {
            if let Some((lhs, _)) = line.split_once("::=") {
                defined.insert(lhs.trim().to_string());
            }
        }
        assert!(defined.contains("root"), "grammar has no `root` rule:\n{grammar}");

        // Strip string literals and char classes, then any leftover identifier is a rule
        // reference that MUST be defined.
        for line in grammar.lines() {
            let Some((_, rhs)) = line.split_once("::=") else { continue };
            let stripped = strip_literals_and_classes(rhs);
            for ident in stripped.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')) {
                if ident.is_empty() || ident.chars().next().is_some_and(|c| c.is_numeric()) {
                    continue;
                }
                assert!(
                    defined.contains(ident),
                    "grammar references undefined rule `{ident}`:\n{grammar}"
                );
            }
        }
    }

    /// Remove `"..."` string literals and `[...]` char classes so only rule-reference
    /// identifiers remain. Handles GBNF `\"` / `\\` escapes inside literals.
    fn strip_literals_and_classes(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    // consume to the closing unescaped quote
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\\' {
                            chars.next(); // skip escaped char
                        } else if n == '"' {
                            break;
                        }
                    }
                }
                '[' => {
                    // consume to the closing `]` (char class); handle `\]` escape
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\\' {
                            chars.next();
                        } else if n == ']' {
                            break;
                        }
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }

    #[test]
    fn generates_envelope_for_a_simple_object_schema() {
        let schema = json!({
            "type": "object",
            "properties": { "q": { "type": "string" }, "limit": { "type": "integer" } },
            "required": ["q"]
        });
        let g = tool_call_grammar(&[("web_search", &schema)]).expect("grammar");
        assert!(g.contains("root ::="), "must have a root rule");
        assert!(g.contains("<tool_call>"), "must force the tool-call envelope");
        assert!(g.contains("\\\"web_search\\\""), "must fix the tool name literal");
        assert_parseable(&g);
    }

    #[test]
    fn covers_every_supported_scalar_and_container_type() {
        let schema = json!({
            "type": "object",
            "properties": {
                "s": { "type": "string" },
                "n": { "type": "number" },
                "i": { "type": "integer" },
                "b": { "type": "boolean" },
                "arr": { "type": "array", "items": { "type": "string" } },
                "mode": { "enum": ["a", "b", "c"] },
                "nested": { "type": "object", "properties": { "x": { "type": "number" } }, "required": ["x"] }
            },
            "required": ["s", "n", "i", "b", "arr", "mode", "nested"]
        });
        let g = tool_call_grammar(&[("all_types", &schema)]).expect("grammar");
        assert_parseable(&g);
        assert!(g.contains("boolean"));
        assert!(g.contains("number"));
    }

    #[test]
    fn multi_tool_set_produces_one_alternative_per_tool() {
        let a = json!({ "type": "object", "properties": { "x": { "type": "string" } }, "required": ["x"] });
        let b = json!({ "type": "object", "properties": {} });
        let g = tool_call_grammar(&[("tool_a", &a), ("tool_b", &b)]).expect("grammar");
        assert!(g.contains("\\\"tool_a\\\""));
        assert!(g.contains("\\\"tool_b\\\""));
        // root alternation joins the two call rules.
        let root = g.lines().find(|l| l.starts_with("root ::=")).unwrap();
        assert!(root.contains(" | "), "multi-tool root must alternate: {root}");
        assert_parseable(&g);
    }

    #[test]
    fn empty_object_schema_falls_back_to_generic_object() {
        let g = tool_call_grammar(&[("noop", &json!({}))]).expect("grammar");
        assert_parseable(&g);
        // `{}` (no type) → args is the generic `value` rule; still a valid envelope.
        assert!(g.contains("root ::="));
    }

    #[test]
    fn empty_tool_set_returns_none() {
        assert!(tool_call_grammar(&[]).is_none(), "no tools → unconstrained fallback");
    }

    #[test]
    fn malformed_schema_is_skipped_never_panics() {
        // `type` outside the subset → the ONLY tool is skipped → whole grammar is None.
        let bad = json!({ "type": "geospatial-polygon" });
        assert!(
            tool_call_grammar(&[("weird", &bad)]).is_none(),
            "unsupported type must skip the tool (fallback), not panic"
        );

        // A non-object schema value is malformed → skipped.
        let not_a_schema = json!("this is a string, not a schema");
        assert!(tool_call_grammar(&[("weird", &not_a_schema)]).is_none());

        // In a mixed set, the good tool survives and the bad one is dropped.
        let good = json!({ "type": "object", "properties": { "x": { "type": "string" } }, "required": ["x"] });
        let g = tool_call_grammar(&[("weird", &bad), ("good", &good)]).expect("grammar");
        assert!(g.contains("\\\"good\\\""));
        assert!(!g.contains("\\\"weird\\\""), "unsupported tool must be dropped");
        assert_parseable(&g);
    }

    #[test]
    fn grammar_does_not_validate_values() {
        // SECURITY BOUNDARY: the grammar fixes the string TYPE of `path` but places no
        // constraint on its VALUE — a traversal like "../../etc/passwd" is grammar-legal.
        // Value rejection is dispatch's path-guard job (P1), deliberately not duplicated.
        let schema = json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] });
        let g = tool_call_grammar(&[("fs_read", &schema)]).expect("grammar");
        // The value rule is the generic `string` — no path-shape restriction whatsoever.
        assert!(g.contains("string"), "path is constrained to a JSON string type only");
        assert!(
            !g.contains("passwd") && !g.contains(".."),
            "grammar must not encode any value-level policy"
        );
    }

    #[test]
    fn result_is_cached_per_tool_set() {
        let schema = json!({ "type": "object", "properties": { "x": { "type": "string" } }, "required": ["x"] });
        let first = tool_call_grammar(&[("cached_tool", &schema)]);
        let second = tool_call_grammar(&[("cached_tool", &schema)]);
        assert_eq!(first, second, "same tool set must resolve identically (cached)");
    }
}
