//! The `PlanDraft` contract — the Design stage's forced-JSON output, plus its JSON schema
//! (for the GBNF grammar), a tolerant parse-and-repair, and the validity invariant.
//!
//! The draft is the load-bearing artifact between Design and Write: Design emits it (as forced
//! JSON via `emit_plan_draft`), the runner's `Gate::Artifact` re-parses it, and Write renders
//! it into `plan.md`/`phase-NN` files. It must survive a weak model — hence `parse_and_repair`
//! (tolerant of ```json fences and trailing prose) is the PRIMARY path off-llama; the GBNF
//! grammar is a llama-only optimization layered on top.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The synthetic tool name the Design stage's grammar and whitelist are built around. It is
/// never a general-purpose tool — its schema exists to shape the forced JSON and its executor
/// persists the draft (see `tools::EmitPlanDraftTool`).
pub const EMIT_PLAN_DRAFT_TOOL: &str = "emit_plan_draft";

/// A structured plan draft: the reviewable "why + what" a weak-model build is bounded by.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanDraft {
    /// The chosen approach in prose.
    pub approach: String,
    /// At least one rejected alternative (the "why not X") — enforced by [`Self::validate`].
    /// A plan with no rejected alternative is not a reviewable decision, so it fails the gate.
    pub rejected: Vec<String>,
    /// The phase decomposition (must be non-empty).
    pub phases: Vec<PhaseSpec>,
    /// Assumption ledger (claim + confidence + verification) — inherited from the depth-tier
    /// design contract. Optional: a plan may legitimately carry no explicit assumptions.
    #[serde(default)]
    pub assumptions: Vec<Assumption>,
}

/// One phase in the plan — the fields that render into the 7-field HailyKit phase frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PhaseSpec {
    pub phase: u32,
    pub title: String,
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default = "default_priority")]
    pub priority: String,
    #[serde(default = "default_effort")]
    pub effort: String,
    #[serde(default)]
    pub dependencies: Vec<u32>,
    #[serde(default = "default_tier")]
    pub tier: String,
}

/// One assumption + how to verify it (claim + confidence + verification command/step).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Assumption {
    pub claim: String,
    #[serde(default = "default_confidence")]
    pub confidence: String,
    #[serde(default)]
    pub verification: String,
}

fn default_status() -> String {
    "pending".to_string()
}
fn default_priority() -> String {
    "P2".to_string()
}
fn default_effort() -> String {
    "1d".to_string()
}
fn default_tier() -> String {
    "medium".to_string()
}
fn default_confidence() -> String {
    "medium".to_string()
}

impl PlanDraft {
    /// The invariant a passing draft must satisfy: at least one rejected alternative AND at
    /// least one phase. A model that emits neither has not produced a reviewable plan.
    ///
    /// # Errors
    /// Returns an error naming the first violated invariant — surfaced to the stage model as
    /// the `emit_plan_draft` tool-result so the next attempt can correct it.
    pub fn validate(&self) -> Result<()> {
        if self.rejected.iter().all(|r| r.trim().is_empty()) {
            bail!("plan draft must include at least one non-empty rejected alternative");
        }
        if self.phases.is_empty() {
            bail!("plan draft must include at least one phase");
        }
        Ok(())
    }
}

/// The JSON schema for [`PlanDraft`] — the input to `tool_call_grammar` (llama constraint) and
/// the `emit_plan_draft` tool's `parameters_schema`. Kept in the GBNF-supported subset (object
/// / array / string / integer) so the grammar generator never skips it.
pub fn plan_draft_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "approach": { "type": "string" },
            "rejected": { "type": "array", "items": { "type": "string" } },
            "phases": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "phase": { "type": "integer" },
                        "title": { "type": "string" },
                        "status": { "type": "string" },
                        "priority": { "type": "string" },
                        "effort": { "type": "string" },
                        "dependencies": { "type": "array", "items": { "type": "integer" } },
                        "tier": { "type": "string" }
                    },
                    "required": ["phase", "title"]
                }
            },
            "assumptions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "claim": { "type": "string" },
                        "confidence": { "type": "string" },
                        "verification": { "type": "string" }
                    },
                    "required": ["claim"]
                }
            }
        },
        "required": ["approach", "rejected", "phases"]
    })
}

/// Build the Design stage's GBNF grammar from the `emit_plan_draft` schema. `None` when the
/// generator cannot construct one (never on this schema, but the caller falls back to
/// unconstrained generation regardless — parse-and-repair is the correctness path).
pub fn design_grammar() -> Option<String> {
    let schema = plan_draft_schema();
    haily_llm::gbnf::tool_call_grammar(&[(EMIT_PLAN_DRAFT_TOOL, &schema)])
}

/// Parse a [`PlanDraft`] from raw model text, tolerating a ```json fence and trailing prose.
/// This is the PRIMARY parse path (GBNF is llama-only): a weak model's JSON is repaired here
/// before it can reach the Write stage.
///
/// # Errors
/// Returns an error when no JSON object is present, the JSON is unparseable, or the parsed
/// draft violates [`PlanDraft::validate`].
pub fn parse_and_repair(raw: &str) -> Result<PlanDraft> {
    let json = extract_json_block(raw);
    let draft: PlanDraft =
        serde_json::from_str(&json).context("parsing PlanDraft JSON (after fence/prose repair)")?;
    draft.validate()?;
    Ok(draft)
}

/// Deserialize a [`PlanDraft`] from already-parsed tool-call `args` (an object), falling back
/// to [`parse_and_repair`] when the model passed a stringified JSON payload instead.
///
/// # Errors
/// Returns an error when the value is neither a valid draft object nor a repairable string.
pub fn draft_from_args(args: &Value) -> Result<PlanDraft> {
    if let Some(s) = args.as_str() {
        return parse_and_repair(s);
    }
    let draft: PlanDraft =
        serde_json::from_value(args.clone()).context("deserializing PlanDraft from tool args")?;
    draft.validate()?;
    Ok(draft)
}

/// Extract the JSON object substring from raw text: strip an optional ```json fence, then keep
/// from the first `{` to the last `}` so trailing prose ("Here is the plan:", "Let me know…")
/// never breaks parsing.
fn extract_json_block(raw: &str) -> String {
    let s = raw.trim();
    // Strip a leading ```lang fence line + trailing ``` if present.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_json() -> &'static str {
        r#"{"approach":"do it","rejected":["big bang rewrite"],
            "phases":[{"phase":1,"title":"First"}],
            "assumptions":[{"claim":"api stable"}]}"#
    }

    #[test]
    fn parses_a_clean_draft_and_applies_field_defaults() {
        let d = parse_and_repair(valid_json()).expect("parse");
        assert_eq!(d.phases.len(), 1);
        // Phase field defaults fill the 7-field frontmatter even when the model omits them.
        assert_eq!(d.phases[0].status, "pending");
        assert_eq!(d.phases[0].priority, "P2");
        assert_eq!(d.phases[0].tier, "medium");
        assert_eq!(d.assumptions[0].confidence, "medium");
    }

    #[test]
    fn repairs_json_fence_and_trailing_prose() {
        let raw = format!(
            "Here is the plan:\n```json\n{}\n```\nLet me know!",
            valid_json()
        );
        let d = parse_and_repair(&raw).expect("repair");
        assert_eq!(d.approach, "do it");
    }

    #[test]
    fn validate_rejects_empty_rejected_and_empty_phases() {
        let mut d = parse_and_repair(valid_json()).unwrap();
        d.rejected = vec!["  ".to_string()];
        assert!(
            d.validate().is_err(),
            "blank rejected alternative must fail"
        );
        let mut d2 = parse_and_repair(valid_json()).unwrap();
        d2.phases.clear();
        assert!(d2.validate().is_err(), "no phases must fail");
    }

    #[test]
    fn design_grammar_forces_the_emit_tool_envelope() {
        let g = design_grammar().expect("grammar builds for the plan-draft schema");
        assert!(g.contains("root ::="));
        assert!(
            g.contains("emit_plan_draft"),
            "grammar must fix the emit tool name"
        );
    }

    #[test]
    fn draft_from_args_accepts_object_and_stringified_payload() {
        let obj: Value = serde_json::from_str(valid_json()).unwrap();
        assert!(
            draft_from_args(&obj).is_ok(),
            "object args must deserialize"
        );
        let str_payload = Value::String(valid_json().to_string());
        assert!(
            draft_from_args(&str_payload).is_ok(),
            "stringified args must be repaired"
        );
    }
}
