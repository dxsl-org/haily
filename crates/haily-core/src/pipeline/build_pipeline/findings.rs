//! Review findings — the independent Review stage's forced-JSON output (Sub-Agent + Skill
//! Architecture P6), its GBNF grammar (SAME mechanism as P5's `emit_plan_draft`), a tolerant
//! parse-and-repair, and the deterministic anti-reward-hacking guard.
//!
//! The findings array is the load-bearing artifact between Review and the Fix loop: the Review
//! sub-turn emits it (grammar-forced via `emit_findings`), its executor persists it to the run's
//! pre-allocated `pipeline_runs.findings` column (P4a forward slot — no migration), and the
//! wrapper reads it back to decide whether a Critical finding routes into the Fix loop.

use anyhow::{Context, Result};
use async_trait::async_trait;
use haily_tools::{RiskTier, Tool, ToolContext};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The synthetic tool name the Review stage's grammar + whitelist are built around (never a
/// general-purpose tool — its schema shapes the forced JSON and its executor persists findings).
pub const EMIT_FINDINGS_TOOL: &str = "emit_findings";

/// Finding severity. Only [`Severity::Critical`] routes into the Fix loop; everything else is
/// logged to the phase's deviation/notes (P6 Architecture). Ordering is severity-descending so
/// `>= Critical` etc. read naturally, but routing keys off the exact `Critical` variant.
///
/// `Deserialize` is IMPLEMENTED MANUALLY (not derived) so it routes through [`Severity::parse`]
/// — review fix (P6): the derived `#[serde(rename_all="lowercase")]` matches ONLY the exact
/// lowercase discriminant, so off-llama (no GBNF enforcement) a single capitalized/typo'd
/// severity (e.g. `"Critical"`, `"crit"`) would fail `serde_json::from_value` for the WHOLE
/// findings array, silently dropping every finding in it. Routing through the already-lenient
/// `parse` makes a typo degrade to a nearby severity instead of vanishing the array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl Severity {
    /// Tolerant parse of a model-supplied severity string (default [`Severity::Medium`] for an
    /// unrecognized value — a weak model's typo must not silently become Critical or vanish).
    pub fn parse(s: &str) -> Severity {
        match s.trim().to_lowercase().as_str() {
            "critical" | "crit" | "blocker" => Severity::Critical,
            "high" => Severity::High,
            "low" => Severity::Low,
            "info" | "informational" | "note" | "nit" => Severity::Info,
            _ => Severity::Medium,
        }
    }
}

impl<'de> Deserialize<'de> for Severity {
    /// Deserializes through a raw `String` + [`Severity::parse`] rather than an exact-match
    /// enum derive, so a capitalized or near-synonym severity from a weak model degrades to the
    /// nearest level instead of failing the whole findings array's deserialization.
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Severity::parse(&s))
    }
}

/// One review finding: what, where, and — critically — the concrete failure it would cause.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    pub severity: Severity,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line: Option<u32>,
    pub summary: String,
    /// The concrete scenario in which this becomes a real fault (forces the reviewer past
    /// "looks fine" into "here is how it breaks in prod").
    #[serde(default)]
    pub failure_scenario: String,
}

impl Finding {
    /// True for a finding that must block ship and route into the Fix loop.
    pub fn is_critical(&self) -> bool {
        self.severity == Severity::Critical
    }
}

/// The JSON schema for the `emit_findings` tool call — the input to `tool_call_grammar` (llama
/// constraint) and the tool's `parameters_schema`. Kept in the GBNF-supported subset.
pub fn emit_findings_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "severity": { "type": "string", "enum": ["critical", "high", "medium", "low", "info"] },
                        "file": { "type": "string" },
                        "line": { "type": "integer" },
                        "summary": { "type": "string" },
                        "failure_scenario": { "type": "string" }
                    },
                    "required": ["severity", "summary"]
                }
            }
        },
        "required": ["findings"]
    })
}

/// Build the Review stage's GBNF grammar from the `emit_findings` schema (SAME mechanism as
/// P5's `design_grammar`). `None` only if the generator cannot build one; the caller falls back
/// to unconstrained generation regardless — [`parse_findings`] is the correctness path.
pub fn findings_grammar() -> Option<String> {
    let schema = emit_findings_schema();
    haily_llm::gbnf::tool_call_grammar(&[(EMIT_FINDINGS_TOOL, &schema)])
}

/// The findings-array wrapper for (de)serialization.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FindingsDoc {
    #[serde(default)]
    pub findings: Vec<Finding>,
}

/// Parse findings from already-parsed tool-call `args`, falling back to a raw-string repair
/// (fence/prose tolerant) when the model passed a stringified payload. An EMPTY findings array
/// is valid — a clean review legitimately reports nothing.
///
/// # Errors
/// Returns an error only when the value is neither a valid `{findings:[...]}` object nor a
/// repairable string.
pub fn parse_findings(args: &Value) -> Result<Vec<Finding>> {
    if let Some(s) = args.as_str() {
        return parse_findings_raw(s);
    }
    let doc: FindingsDoc =
        serde_json::from_value(args.clone()).context("deserializing findings from tool args")?;
    Ok(doc.findings)
}

/// Repair findings JSON from raw model text (tolerating a ```json fence and trailing prose).
fn parse_findings_raw(raw: &str) -> Result<Vec<Finding>> {
    let json = extract_json_block(raw);
    let doc: FindingsDoc =
        serde_json::from_str(&json).context("parsing findings JSON (after fence/prose repair)")?;
    Ok(doc.findings)
}

/// Strip a ```json fence and keep from the first `{` to the last `}` (identical repair to P5's
/// plan-draft path — trailing prose never breaks parsing).
fn extract_json_block(raw: &str) -> String {
    let s = raw.trim();
    let s = if let Some(rest) = s.strip_prefix("```") {
        let after_lang = rest.split_once('\n').map(|x| x.1).unwrap_or(rest);
        after_lang
            .rsplit_once("```")
            .map(|(body, _)| body)
            .unwrap_or(after_lang)
    } else {
        s
    };
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b >= a => s[a..=b].to_string(),
        _ => s.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Anti-reward-hacking guard (P6 LOCKED decision #2, harness-article adopt).
// ---------------------------------------------------------------------------

/// Assertion-macro prefixes checked on a REMOVED line (any file, any location) — review fix
/// (P6): this repo's DOMINANT test convention is an inline `#[cfg(test)] mod tests { ... }`
/// block inside an ordinary `src/*.rs` file, which [`is_test_path`]'s filename/directory
/// heuristic cannot see. Flagging a removed assertion macro regardless of file closes that gap
/// without needing to parse `#[cfg(test)]` boundaries out of a unified diff — the fix loop
/// should never legitimately need to delete an assertion to pass a gate.
const ASSERT_MACROS: &[&str] = &[
    "assert!(",
    "assert_eq!(",
    "assert_ne!(",
    "debug_assert!(",
    "debug_assert_eq!(",
    "debug_assert_ne!(",
];

/// True iff an ADDED line is a trivially-true assertion (`assert!(true)`, `assert!(1==1)`,
/// `assert_eq!(1,1)`) regardless of surrounding whitespace — the other half of the inline-test
/// blind spot: a fix can leave the assertion MACRO in place while hollowing out its condition.
fn is_trivially_true_assertion(added_line: &str) -> bool {
    let compact: String = added_line.chars().filter(|c| !c.is_whitespace()).collect();
    matches!(
        compact.as_str(),
        "assert!(true)"
            | "assert!(true);"
            | "assert!(1==1)"
            | "assert!(1==1);"
            | "assert_eq!(1,1)"
            | "assert_eq!(1,1);"
    )
}

/// Deterministic guard: a fix that turns a gate red→green by tampering with the TEST rather than
/// the code is a Failure, not a pass. Inspects a fix's unified `diff` and returns a synthetic
/// Critical [`Finding`] when it: touched a test file; added a test-suppression token
/// (`#[ignore]`, `.skip(`, `.only(`, `xfail`, a `@pytest.mark.skip`); added a trivially-true
/// assertion; or removed an assertion macro line — the last two apply to ANY file (not just a
/// conventionally-named test file), so weakening an assertion inside this repo's inline
/// `#[cfg(test)] mod tests` blocks in an ordinary `src/*.rs` file is caught too. Heuristic +
/// surfaced (the diff is shown for review), never a silent hard block — the model optimizes
/// whatever signal it is given, so a green gate that only went green because its test was
/// weakened must be caught.
///
/// Pure over the diff string so it is unit-testable without a workspace.
pub fn detect_test_tampering(diff: &str) -> Option<Finding> {
    let mut touched_test_files: Vec<String> = Vec::new();
    let mut suppression_hits: Vec<&str> = Vec::new();
    let mut added_trivial_assert = false;
    let mut removed_assert_line: Option<String> = None;
    const SUPPRESSIONS: &[&str] = &[
        "#[ignore]",
        ".skip(",
        ".only(",
        "xfail",
        "pytest.mark.skip",
        "@Disabled",
        "it.skip",
        "test.skip",
    ];

    for line in diff.lines() {
        // A file header names the file the following hunk edits.
        if let Some(path) = line
            .strip_prefix("+++ b/")
            .or_else(|| line.strip_prefix("--- a/"))
        {
            let path = path.trim();
            if path != "/dev/null"
                && is_test_path(path)
                && !touched_test_files.contains(&path.to_string())
            {
                touched_test_files.push(path.to_string());
            }
            continue;
        }
        if let Some(added) = line.strip_prefix('+') {
            if added.starts_with("++") {
                continue; // file header, handled above
            }
            // Only ADDED lines can introduce a suppression; a removed one is the fix, not the hack.
            for s in SUPPRESSIONS {
                if added.contains(s) && !suppression_hits.contains(s) {
                    suppression_hits.push(s);
                }
            }
            if !added_trivial_assert && is_trivially_true_assertion(added) {
                added_trivial_assert = true;
            }
            continue;
        }
        if let Some(removed) = line.strip_prefix('-') {
            if removed.starts_with("--") {
                continue; // file header ("--- a/..." or the "/dev/null" delete-file variant)
            }
            if removed_assert_line.is_none() && ASSERT_MACROS.iter().any(|m| removed.contains(m)) {
                removed_assert_line = Some(removed.trim().to_string());
            }
        }
    }

    if touched_test_files.is_empty()
        && suppression_hits.is_empty()
        && !added_trivial_assert
        && removed_assert_line.is_none()
    {
        return None;
    }
    let mut summary = String::from("reward-hacking guard: the fix altered the TEST, not the code");
    if !touched_test_files.is_empty() {
        summary.push_str(&format!(
            " — modified test file(s): {}",
            touched_test_files.join(", ")
        ));
    }
    if !suppression_hits.is_empty() {
        summary.push_str(&format!(
            " — added test-suppression token(s): {}",
            suppression_hits.join(", ")
        ));
    }
    if added_trivial_assert {
        summary.push_str(" — added a trivially-true assertion (e.g. `assert!(true)`)");
    }
    if let Some(removed) = &removed_assert_line {
        summary.push_str(&format!(" — removed an assertion: {removed}"));
    }
    Some(Finding {
        severity: Severity::Critical,
        file: touched_test_files.first().cloned().unwrap_or_default(),
        line: None,
        summary,
        failure_scenario: "A gate that went green only because its test was weakened is a false \
                           pass — the untested behavior can still be broken in production."
            .to_string(),
    })
}

/// Path conventions that mark a file as a test file (Rust/JS/TS/Python).
fn is_test_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    let base = p.rsplit('/').next().unwrap_or(&p);
    p.contains("/tests/")
        || p.starts_with("tests/")
        || base.ends_with("_test.rs")
        || base.ends_with("_test.go")
        || base.ends_with("_spec.rb")
        || base.starts_with("test_")
        || base.contains(".test.")
        || base.contains(".spec.")
        || base.contains("_spec.")
}

/// The Review stage's synthetic tool: accepts findings as JSON (grammar-forced or repaired) and
/// persists them onto the active run's `pipeline_runs.findings` column, so the wrapper can read
/// them back to drive the Fix loop. Stateless w.r.t. phase/round — it resolves the run from
/// `ctx.run_id` (set only for a pipeline stage sub-turn), so ONE instance serves every phase.
pub struct EmitFindingsTool;

#[async_trait]
impl Tool for EmitFindingsTool {
    fn name(&self) -> &str {
        EMIT_FINDINGS_TOOL
    }
    fn description(&self) -> &str {
        "Record independent review findings (severity, file, line, summary, failure_scenario) as JSON."
    }
    fn parameters_schema(&self) -> Value {
        emit_findings_schema()
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let findings = parse_findings(&args)?;
        let doc = FindingsDoc {
            findings: findings.clone(),
        };
        let serialized = serde_json::to_string(&doc).context("serializing findings")?;
        // Persist onto the run row (P6 LOCKED decision #3: the pre-allocated nullable column).
        // A review stage always carries a `run_id`; without one (a mis-wired call) we still
        // return Ok so the sub-turn completes, but nothing is persisted for the wrapper to read.
        if let Some(run_id) = &ctx.run_id {
            haily_db::queries::pipeline_runs::set_findings(&ctx.db, run_id, &serialized)
                .await
                .context("persisting findings to pipeline_runs")?;
        } else {
            tracing::warn!("emit_findings called outside a pipeline run — findings not persisted");
        }
        let crit = findings.iter().filter(|f| f.is_critical()).count();
        Ok(format!(
            "recorded {} finding(s), {} critical",
            findings.len(),
            crit
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_object_and_stringified_findings() {
        let obj = json!({ "findings": [
            { "severity": "critical", "file": "src/a.rs", "line": 10, "summary": "unwrap in prod", "failure_scenario": "panics on None" }
        ]});
        let f = parse_findings(&obj).expect("object");
        assert_eq!(f.len(), 1);
        assert!(f[0].is_critical());

        let s = Value::String(
            r#"```json
{"findings":[{"severity":"low","summary":"nit"}]}
``` trailing prose"#
                .to_string(),
        );
        let f2 = parse_findings(&s).expect("stringified repair");
        assert_eq!(f2[0].severity, Severity::Low);
    }

    #[test]
    fn empty_findings_is_a_valid_clean_review() {
        let f = parse_findings(&json!({ "findings": [] })).expect("empty");
        assert!(f.is_empty(), "a clean review reports no findings");
    }

    #[test]
    fn severity_parse_defaults_unknown_to_medium() {
        assert_eq!(Severity::parse("Critical"), Severity::Critical);
        assert_eq!(Severity::parse("blocker"), Severity::Critical);
        assert_eq!(Severity::parse("wat"), Severity::Medium);
    }

    #[test]
    fn parse_findings_tolerates_capitalized_or_typo_severity_without_failing_the_whole_array() {
        // Review fix (P6): off-llama, the derived exact-match Deserialize would fail the WHOLE
        // array on one capitalized/typo'd severity, silently dropping every finding in it —
        // Severity's custom Deserialize must route through the lenient `parse` instead.
        let v = json!({ "findings": [
            { "severity": "Critical", "summary": "capitalized severity" },
            { "severity": "crit", "summary": "synonym severity" },
            { "severity": "totally-unknown-typo", "summary": "unrecognized token" }
        ]});
        let findings =
            parse_findings(&v).expect("a capitalized/typo severity must not fail the whole array");
        assert_eq!(
            findings.len(),
            3,
            "every finding must survive, none silently dropped"
        );
        assert!(
            findings[0].is_critical(),
            "\"Critical\" (capitalized) must resolve to Critical"
        );
        assert!(
            findings[1].is_critical(),
            "\"crit\" synonym must resolve to Critical"
        );
        assert_eq!(
            findings[2].severity,
            Severity::Medium,
            "unknown token falls back, not dropped"
        );
    }

    #[test]
    fn findings_grammar_forces_the_emit_tool_envelope() {
        let g = findings_grammar().expect("grammar builds");
        assert!(g.contains("root ::="));
        assert!(
            g.contains("emit_findings"),
            "grammar must fix the emit tool name"
        );
    }

    #[test]
    fn reward_hack_guard_flags_a_modified_test_file() {
        let diff = "\
--- a/crates/core/src/lib.rs
+++ b/crates/core/src/lib.rs
@@
+fn real_fix() {}
--- a/crates/core/tests/behavior_test.rs
+++ b/crates/core/tests/behavior_test.rs
@@
-    assert_eq!(compute(), 42);
+    assert!(true);
";
        let finding = detect_test_tampering(diff).expect("must flag test-file tampering");
        assert!(finding.is_critical(), "test tampering is a Critical");
        assert!(finding.summary.contains("behavior_test.rs"));
    }

    #[test]
    fn reward_hack_guard_flags_added_ignore_and_skip_tokens() {
        let diff = "\
--- a/src/foo.rs
+++ b/src/foo.rs
@@
+#[ignore]
 fn t() {}
";
        assert!(
            detect_test_tampering(diff).is_some(),
            "an added #[ignore] must be flagged"
        );

        let js = "\
--- a/src/foo.js
+++ b/src/foo.js
@@
+it.skip('does the thing', () => {})
";
        assert!(
            detect_test_tampering(js).is_some(),
            "an added .skip( must be flagged"
        );
    }

    #[test]
    fn reward_hack_guard_passes_a_clean_code_only_fix() {
        let diff = "\
--- a/src/foo.rs
+++ b/src/foo.rs
@@
-    let x = risky.unwrap();
+    let x = risky?;
";
        assert!(
            detect_test_tampering(diff).is_none(),
            "a real code fix must not be flagged"
        );
    }

    #[test]
    fn reward_hack_guard_flags_inline_cfg_test_assertion_weakening_in_a_src_file() {
        // This repo's DOMINANT test convention is #[cfg(test)] mod tests inside an ordinary
        // src/*.rs file — is_test_path's filename heuristic cannot see this. The removed/added
        // assertion checks apply regardless of file, closing the blind spot without parsing
        // #[cfg(test)] boundaries out of the diff.
        let diff = "\
--- a/crates/core/src/lib.rs
+++ b/crates/core/src/lib.rs
@@
 #[cfg(test)]
 mod tests {
     #[test]
     fn it_computes() {
-        assert_eq!(compute(), 42);
+        assert!(true);
     }
 }
";
        let finding = detect_test_tampering(diff)
            .expect("inline #[cfg(test)] assertion weakening in a src file must be flagged");
        assert!(finding.is_critical());
        assert!(finding.summary.contains("removed an assertion"));
        assert!(finding.summary.contains("trivially-true"));
    }

    #[test]
    fn reward_hack_guard_flags_only_a_hollowed_assertion_with_no_suppression_token() {
        // The narrowest reproduction of the gap: no test-file path, no #[ignore]/.skip(, just
        // the assertion itself weakened in place.
        let diff = "\
--- a/src/math.rs
+++ b/src/math.rs
@@
-    assert_eq!(add(2, 2), 4);
+    assert!(1==1);
";
        let finding =
            detect_test_tampering(diff).expect("a hollowed assertion alone must be flagged");
        assert!(finding.is_critical());
    }
}
