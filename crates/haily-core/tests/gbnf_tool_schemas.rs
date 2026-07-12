//! Phase 3 golden sweep: every real tool schema in `ToolRegistry::build_v1()` (all v1
//! personal tools + the coding tool surface) must produce a GBNF grammar via
//! `haily_llm::gbnf::tool_call_grammar` without panicking.
//!
//! This lives in haily-core (not haily-llm) because haily-llm is a leaf crate that
//! cannot depend on haily-tools — haily-core is the layer where both the real tool
//! registry and the GBNF generator are visible together. The generator's own
//! type-coverage / malformed-schema unit tests live in `haily-llm/src/gbnf.rs`.

use haily_llm::gbnf;
use haily_tools::ToolRegistry;

/// Lightweight structural check mirroring the one in `gbnf.rs` tests: a `root` rule
/// exists. (Full parseability is asserted per-type inside the generator's unit tests;
/// here we assert the whole real tool surface is expressible without a panic.)
fn has_root(grammar: &str) -> bool {
    grammar.lines().any(|l| l.trim_start().starts_with("root ::="))
}

#[test]
fn every_v1_and_coding_tool_schema_produces_a_grammar() {
    let registry = ToolRegistry::build_v1();
    let tools = registry.list();
    assert!(!tools.is_empty(), "build_v1 must register tools");

    for tool in &tools {
        let name = tool.name().to_string();
        let schema = tool.parameters_schema();
        // Per-tool grammar: proves each individual schema is in the supported subset (or
        // is cleanly skippable). A None here means the schema is outside the subset —
        // acceptable (unconstrained fallback), but must NEVER panic.
        if let Some(grammar) = gbnf::tool_call_grammar(&[(name.as_str(), &schema)]) {
            assert!(has_root(&grammar), "tool `{name}` produced a grammar with no root:\n{grammar}");
            assert!(
                grammar.contains("<tool_call>"),
                "tool `{name}` grammar must force the tool-call envelope"
            );
        }
    }
}

#[test]
fn full_tool_set_grammar_is_generated_and_alternates() {
    let registry = ToolRegistry::build_v1();
    let tools = registry.list();
    // Own the (name, schema) pairs so the borrows into `tool_call_grammar` are valid.
    let owned: Vec<(String, serde_json::Value)> =
        tools.iter().map(|t| (t.name().to_string(), t.parameters_schema())).collect();
    let refs: Vec<(&str, &serde_json::Value)> =
        owned.iter().map(|(n, s)| (n.as_str(), s)).collect();

    let grammar = gbnf::tool_call_grammar(&refs).expect("the full v1 tool set must yield a grammar");
    assert!(has_root(&grammar));
    let root = grammar.lines().find(|l| l.trim_start().starts_with("root ::=")).unwrap();
    assert!(root.contains(" | "), "root must alternate across many tools: {root}");
}
