//! Deterministic renderer: [`PlanDraft`] → HailyKit-compatible `plan.md` + `phase-NN-*.md`
//! files. Keeping the render in Rust (not the model) is the harness contract — a weak model
//! cannot reliably emit the EXACT 7-field frontmatter, so the pipeline's Write stage delegates
//! the byte-level rendering to this module (see `tools::RenderPlanTool`).
//!
//! Frontmatter contract (matches every shipped phase file EXACTLY, e.g.
//! `phase-04-pipeline-engine-core.md`): `phase` (int), `title` (quoted), `status`, `priority`,
//! `effort`, `dependencies` (int array), `tier`.

use super::draft::{PhaseSpec, PlanDraft};

/// Max slug length for a phase filename — keeps `phase-NN-<slug>.md` a sane length.
const MAX_SLUG_LEN: usize = 40;

/// The workspace-relative artifacts the Write stage renders: `plan.md`, one `phase-NN-*.md`
/// per phase, and the scout report. Paths are all under `.agents/<slug>/` (the HailyKit plan
/// dir), so they are reverted by the worktree compensator like any workspace write.
pub fn plan_artifacts(task: &str, slug: &str, draft: &PlanDraft) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(draft.phases.len() + 2);
    out.push((format!(".agents/{slug}/plan.md"), render_plan_md(task, draft)));
    for p in &draft.phases {
        let rel = format!(".agents/{slug}/phase-{:02}-{}.md", p.phase, slugify(&p.title));
        out.push((rel, render_phase_md(p)));
    }
    out.push((
        format!(".agents/{slug}/reports/scout-report.md"),
        render_scout_report(task, draft),
    ));
    out
}

/// Render the plan overview: approach, rejected alternatives, the phase list, and the
/// assumption ledger. Generic and <80 lines by construction (one line per phase/assumption).
pub fn render_plan_md(task: &str, draft: &PlanDraft) -> String {
    let mut s = String::new();
    s.push_str("# Plan\n\n");
    s.push_str(&format!("## Task\n\n{}\n\n", task.trim()));
    s.push_str(&format!("## Approach\n\n{}\n\n", draft.approach.trim()));

    s.push_str("## Rejected Alternatives\n\n");
    for r in &draft.rejected {
        if !r.trim().is_empty() {
            s.push_str(&format!("- {}\n", r.trim()));
        }
    }
    s.push('\n');

    s.push_str("## Phases\n\n");
    for p in &draft.phases {
        let deps = if p.dependencies.is_empty() {
            "none".to_string()
        } else {
            p.dependencies.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
        };
        s.push_str(&format!(
            "- **Phase {}: {}** — {} · {} · deps: {} · tier: {} → `phase-{:02}-{}.md`\n",
            p.phase,
            p.title.trim(),
            p.priority,
            p.effort,
            deps,
            p.tier,
            p.phase,
            slugify(&p.title),
        ));
    }
    s.push('\n');

    if !draft.assumptions.is_empty() {
        s.push_str("## Assumptions\n\n");
        for a in &draft.assumptions {
            let verify = if a.verification.trim().is_empty() {
                String::new()
            } else {
                format!(" (verify: {})", a.verification.trim())
            };
            s.push_str(&format!("- {} [{}]{}\n", a.claim.trim(), a.confidence, verify));
        }
        s.push('\n');
    }
    s
}

/// Render ONE phase file with the exact 7-field frontmatter, then a minimal body.
pub fn render_phase_md(p: &PhaseSpec) -> String {
    let deps = p.dependencies.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
    format!(
        "---\n\
         phase: {phase}\n\
         title: \"{title}\"\n\
         status: {status}\n\
         priority: {priority}\n\
         effort: {effort}\n\
         dependencies: [{deps}]\n\
         tier: {tier}\n\
         ---\n\
         \n\
         # Phase {phase}: {title_body}\n\
         \n\
         ## Overview\n\
         \n\
         {title_body}\n",
        phase = p.phase,
        title = escape_frontmatter_title(&p.title),
        status = p.status,
        priority = p.priority,
        effort = p.effort,
        deps = deps,
        tier = p.tier,
        title_body = p.title.trim(),
    )
}

/// The scout report the Write stage persists alongside the plan (orientation record). The Scout
/// stage is read-only and cannot write, so the Write stage materializes this from the draft's
/// approach + the task — a durable record of what the plan was oriented on.
fn render_scout_report(task: &str, draft: &PlanDraft) -> String {
    format!(
        "# Scout Report\n\n## Task\n\n{}\n\n## Orientation\n\n{}\n\n## Phase Count\n\n{} phase(s) planned.\n",
        task.trim(),
        draft.approach.trim(),
        draft.phases.len(),
    )
}

/// Escape a title for a double-quoted YAML frontmatter scalar (only `"` and `\` need it).
fn escape_frontmatter_title(title: &str) -> String {
    title.trim().replace('\\', "\\\\").replace('"', "\\\"")
}

/// Filename-safe slug: lowercase, non-alphanumeric → `-`, collapsed, trimmed, length-capped.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    let capped: String = trimmed.chars().take(MAX_SLUG_LEN).collect();
    let capped = capped.trim_end_matches('-').to_string();
    if capped.is_empty() {
        "phase".to_string()
    } else {
        capped
    }
}

#[cfg(test)]
mod tests {
    use super::super::draft::{Assumption, PhaseSpec, PlanDraft};
    use super::*;

    fn draft() -> PlanDraft {
        PlanDraft {
            approach: "incremental".to_string(),
            rejected: vec!["big-bang rewrite".to_string()],
            phases: vec![
                PhaseSpec {
                    phase: 1,
                    title: "Scout & Design".to_string(),
                    status: "pending".to_string(),
                    priority: "P1".to_string(),
                    effort: "2d".to_string(),
                    dependencies: vec![],
                    tier: "thinking".to_string(),
                },
                PhaseSpec {
                    phase: 2,
                    title: "Build It".to_string(),
                    status: "pending".to_string(),
                    priority: "P2".to_string(),
                    effort: "3d".to_string(),
                    dependencies: vec![1],
                    tier: "medium".to_string(),
                },
            ],
            assumptions: vec![Assumption {
                claim: "schema stable".to_string(),
                confidence: "high".to_string(),
                verification: "cargo check".to_string(),
            }],
        }
    }

    #[test]
    fn phase_frontmatter_has_exactly_the_seven_fields() {
        let md = render_phase_md(&draft().phases[1]);
        for field in ["phase: 2", "title: \"Build It\"", "status: pending", "priority: P2", "effort: 3d", "dependencies: [1]", "tier: medium"] {
            assert!(md.contains(field), "phase frontmatter missing `{field}`:\n{md}");
        }
        // Re-parseable as an authored-skill-style frontmatter block (opens + closes with ---).
        assert!(md.starts_with("---\n"));
        assert!(md.contains("\n---\n"), "frontmatter must terminate");
    }

    #[test]
    fn plan_md_lists_phases_rejected_and_assumptions() {
        let md = render_plan_md("do the work", &draft());
        assert!(md.contains("## Rejected Alternatives"));
        assert!(md.contains("big-bang rewrite"));
        assert!(md.contains("Phase 1: Scout & Design"));
        assert!(md.contains("## Assumptions"));
        assert!(md.contains("schema stable"));
    }

    #[test]
    fn artifacts_cover_plan_all_phases_and_the_scout_report() {
        let arts = plan_artifacts("t", "251101-plan", &draft());
        assert!(arts.iter().any(|(p, _)| p == ".agents/251101-plan/plan.md"));
        assert!(arts.iter().any(|(p, _)| p == ".agents/251101-plan/phase-01-scout-design.md"));
        assert!(arts.iter().any(|(p, _)| p == ".agents/251101-plan/phase-02-build-it.md"));
        assert!(arts.iter().any(|(p, _)| p == ".agents/251101-plan/reports/scout-report.md"));
    }

    #[test]
    fn slugify_is_filename_safe() {
        assert_eq!(slugify("Scout & Design!"), "scout-design");
        assert_eq!(slugify("  weird///name  "), "weird-name");
        assert_eq!(slugify("???"), "phase", "an all-symbol title falls back to a stable stem");
    }
}
