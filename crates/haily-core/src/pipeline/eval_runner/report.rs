//! Eval report renderer (Sub-Agent + Skill Architecture phase 9) — the human-facing markdown
//! written to `.agents/reports/eval-<ts>.md` alongside the persisted `eval_runs` row.

use super::scoring::ScoreResult;
use super::EvalOutcome;

/// Render one eval outcome as a markdown report section. Deterministic given the outcome (the
/// caller supplies the timestamp header separately so the body stays reproducible for tests).
pub fn render_outcome(outcome: &EvalOutcome) -> String {
    let mut s = String::new();
    s.push_str(&format!("## Eval: {}\n\n", outcome.task_id));
    s.push_str(&format!("- Model: `{}`\n", outcome.model));
    s.push_str(&format!("- Tier config: `{}`\n", outcome.tier_config));
    s.push_str(&format!("- Depth: `{}`\n", outcome.depth));
    s.push_str(&format!("- Escalations: {}\n", outcome.escalation_count));
    s.push_str(&format!("- Wall clock: {} ms\n", outcome.wall_clock_ms));
    s.push_str(&format!("- Egress: {}\n", outcome.egress_summary()));
    s.push_str(&format!(
        "- **Verdict: {}**\n\n",
        if outcome.score.passed { "PASS" } else { "FAIL" }
    ));
    s.push_str(&render_gates(&outcome.score));
    s
}

/// The gate results as a markdown table.
fn render_gates(score: &ScoreResult) -> String {
    let mut s = String::from("| Gate | Result | Detail |\n|------|--------|--------|\n");
    for g in &score.gates {
        s.push_str(&format!(
            "| {} | {} | {} |\n",
            g.gate,
            if g.pass { "PASS" } else { "FAIL" },
            g.detail
        ));
    }
    s
}

/// Wrap a set of rendered outcome sections into a full report document with a title header.
pub fn render_report(title: &str, sections: &[String]) -> String {
    let mut s = format!("# {title}\n\n");
    if sections.is_empty() {
        s.push_str("_No eval tasks were run._\n");
    }
    for section in sections {
        s.push_str(section);
        s.push_str("\n\n");
    }
    s
}
