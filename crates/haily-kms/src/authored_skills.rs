//! In-memory authored-skill registry: file-sourced skills/standards exported from
//! HailyKit (a "kit-pack"), coexisting with the DB-synthesized skills that live in
//! `kms_skills`.
//!
//! # Why in-memory, not a DB table (LOCKED — see phase-02)
//! Authored skills are the trusted, human-reviewed, sha256-pinned kit-pack; their
//! source of truth is the files on disk. They must NEVER flow through
//! `synthesize_skills_from_traces` / `apply_skill_decay` or the `kms_skills` table
//! (which has its own UNIQUE(name) index and EMA/decay lifecycle) — mixing the two
//! would erase the signed/reviewable/re-exportable properties of the kit-pack and risk
//! name collisions with the synthesized lifecycle. So this is a read-only in-memory
//! index over the files, entirely separate from the synthesized path.
//!
//! # Progressive disclosure (the "no load-all" contract)
//! 1. Index — `name` + `when_to_use`, always present in the L0 routing table.
//! 2. Body — a skill's stage-prompt body, injected only when the skill is matched into
//!    a sub-turn or invoked.
//! 3. References — each `references/` chunk is a SEPARATE [`ReferenceChunk`], loaded one
//!    at a time via `fetch_section`. They are NEVER concatenated into the body — a
//!    full-dump is the forbidden anti-pattern.

use crate::skills::{jaccard_similarity, SkillGates};
use crate::system_prompt::strip_tool_tags;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// The section id of a skill's own body (progressive-disclosure level 2). Reserved —
/// a reference chunk may not reuse this id.
pub const BODY_SECTION: &str = "body";

/// What flavour of authored content a skill is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillKind {
    /// A pipeline-stage prompt (plan/cook/review/…): injected as a `## Playbooks` body.
    StagePrompt,
    /// A reusable how-to playbook — same injection path as a stage prompt.
    Playbook,
    /// A language/framework standard: injected as a `## Standards` body, stack-matched.
    Standard,
}

impl SkillKind {
    /// Parse the frontmatter `kind:` scalar. Returns `None` for an unknown value so the
    /// caller can reject the skill rather than silently defaulting it.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "stage-prompt" => Some(SkillKind::StagePrompt),
            "playbook" => Some(SkillKind::Playbook),
            "standard" => Some(SkillKind::Standard),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SkillKind::StagePrompt => "stage-prompt",
            SkillKind::Playbook => "playbook",
            SkillKind::Standard => "standard",
        }
    }

    /// Whether this kind is injected as a `## Playbooks` body (vs a `## Standards` body).
    fn is_playbook(&self) -> bool {
        matches!(self, SkillKind::StagePrompt | SkillKind::Playbook)
    }
}

/// Index-level view of one authored skill for the cockpit skills browser (phase 11a) —
/// the frontmatter fields only, never the body or reference chunks (progressive
/// disclosure). Authored skills are trusted, sha256-pinned kit-pack content, so they have
/// no EMA confidence/use-count lifecycle the way synthesized skills do; the browser shows
/// them as "authored" with those columns empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredSkillInfo {
    pub name: String,
    pub description: String,
    pub when_to_use: String,
    /// The `SkillKind` as a stable string (`stage-prompt`/`playbook`/`standard`).
    pub kind: String,
}

/// One on-demand reference chunk (progressive-disclosure level 3). Its `body` is loaded
/// only when `fetch_section` pulls it — never injected alongside the skill body.
#[derive(Debug, Clone)]
pub struct ReferenceChunk {
    pub section_id: String,
    pub summary: String,
    pub body: String,
}

/// A single authored skill: frontmatter index + body + reference chunks.
#[derive(Debug, Clone)]
pub struct AuthoredSkill {
    pub name: String,
    pub description: String,
    pub when_to_use: String,
    /// L1 domain this skill routes to (e.g. `developer`). `None` = domain-agnostic; a
    /// domain-filtered lookup only returns a skill whose domain matches exactly, so a
    /// `None`-domain skill never leaks into a specific-domain sub-turn.
    pub domain: Option<String>,
    pub specialists: Vec<String>,
    pub kind: SkillKind,
    pub body: String,
    pub references: Vec<ReferenceChunk>,
}

impl AuthoredSkill {
    /// Parse an authored skill from a `---`-delimited markdown file. `name_fallback`
    /// (the file stem) is used only when the frontmatter omits `name:`.
    ///
    /// Hand-rolled parser for the controlled kit-pack frontmatter (scalars +
    /// `specialists: [..]`) — see phase-02 Deviation D2 for why no YAML crate.
    ///
    /// # Errors
    /// Returns an error when the frontmatter block is absent/unterminated, or a required
    /// scalar (`description`, `when_to_use`, a valid `kind`) is missing/unparseable.
    pub fn from_markdown(name_fallback: &str, raw: &str) -> Result<Self> {
        let (front, body) = split_frontmatter(raw)?;
        let fields = parse_scalars(&front);

        let get = |k: &str| fields.get(k).map(|s| s.trim().to_string());

        let name = get("name")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| name_fallback.to_string());
        let description = get("description")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("skill '{name}': missing 'description'"))?;
        let when_to_use = get("when_to_use")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("skill '{name}': missing 'when_to_use'"))?;
        let kind_raw = get("kind").ok_or_else(|| anyhow!("skill '{name}': missing 'kind'"))?;
        let kind =
            SkillKind::parse(&kind_raw).ok_or_else(|| anyhow!("skill '{name}': invalid kind '{kind_raw}'"))?;
        let domain = get("domain").filter(|s| !s.is_empty());
        let specialists = fields
            .get("specialists")
            .map(|v| parse_list(v))
            .unwrap_or_default();

        Ok(AuthoredSkill {
            name,
            description,
            when_to_use,
            domain,
            specialists,
            kind,
            body: body.trim().to_string(),
            references: Vec::new(),
        })
    }

    /// Stricter check the CI frontmatter validator runs over shipped kit-pack content:
    /// on top of the parse-time required scalars, a kit-pack skill MUST declare a
    /// non-empty `domain` (the routing key). A generic (user/global) skill may omit it,
    /// which is why this is separate from `from_markdown`'s baseline validation.
    ///
    /// # Errors
    /// Returns an error naming the first missing/empty field.
    pub fn validate_complete(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(anyhow!("authored skill: empty name"));
        }
        if self.domain.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Err(anyhow!("skill '{}': missing 'domain'", self.name));
        }
        Ok(())
    }

    /// The concatenation used as this skill's relevance-match text (name + description +
    /// when_to_use) — the same fields the L0 index shows, so the model routes on intent,
    /// not body internals.
    fn match_text(&self) -> String {
        format!("{} {} {}", self.name, self.description, self.when_to_use)
    }

    /// Reconstruct this skill's file bytes (Unified Chat UI phase 8, D4) — a deterministic
    /// re-serialization from the parsed fields, not a byte-exact copy of whatever formatting
    /// the original file happened to use. Used by the skill editor's atomic write/version-
    /// snapshot path when only `body` changed (frontmatter is carried through unchanged).
    ///
    /// Frontmatter scalars are single-line by this hand-rolled format's construction
    /// (`split_once(':')` per line — see `parse_scalars`), so any embedded newline is
    /// flattened to a space here to guarantee the written frontmatter block stays parseable
    /// (a synthesized skill's `description`, which `promote_to_authored` maps into this
    /// struct, has no such guarantee at its source).
    pub fn to_markdown(&self) -> String {
        let mut front = format!(
            "---\nname: {}\ndescription: {}\nwhen_to_use: {}\n",
            sanitize_scalar(&self.name),
            sanitize_scalar(&self.description),
            sanitize_scalar(&self.when_to_use),
        );
        if let Some(domain) = &self.domain {
            front.push_str(&format!("domain: {}\n", sanitize_scalar(domain)));
        }
        front.push_str(&format!("kind: {}\n", self.kind.as_str()));
        if !self.specialists.is_empty() {
            let specialists = self.specialists.iter().map(|s| sanitize_scalar(s)).collect::<Vec<_>>();
            front.push_str(&format!("specialists: [{}]\n", specialists.join(", ")));
        }
        front.push_str("---\n\n");
        format!("{front}{}\n", self.body.trim())
    }
}

/// Flatten a frontmatter scalar to one line — replaces any newline with a space and trims. See
/// `to_markdown`'s doc comment for why this matters.
fn sanitize_scalar(s: &str) -> String {
    s.replace(['\n', '\r'], " ").trim().to_string()
}

/// An immutable point-in-time view of the merged registry. A hot-reload builds a fresh
/// `Snapshot` and swaps the `Arc`; in-flight reads keep the clone they took.
#[derive(Debug, Default)]
struct Snapshot {
    by_name: HashMap<String, AuthoredSkill>,
}

/// In-memory authored-skill registry with 5-tier precedence and version-counter
/// hot-reload.
pub struct AuthoredRegistry {
    snapshot: RwLock<Arc<Snapshot>>,
    /// Bumped on every `reload` — a cache/consumer can compare it to detect a kit-pack
    /// swap without a restart (mirrors the live-router-reload pattern).
    version: AtomicU64,
}

impl Default for AuthoredRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthoredRegistry {
    pub fn new() -> Self {
        AuthoredRegistry {
            snapshot: RwLock::new(Arc::new(Snapshot::default())),
            version: AtomicU64::new(0),
        }
    }

    /// Build from precedence tiers ordered LOWEST-precedence first. A skill name in a
    /// later (higher-precedence) tier OVERRIDES the same name in an earlier one — the
    /// plan's resolution order is workspace > project > personal > global > kit-pack, so
    /// the caller passes `[kit_pack, global, personal, project, workspace]`. Today only
    /// the kit-pack tier is populated, but the merge is implemented in full.
    pub fn from_tiers(tiers: Vec<Vec<AuthoredSkill>>) -> Self {
        let reg = Self::new();
        reg.reload(tiers);
        reg
    }

    /// Atomically swap in a freshly-merged snapshot and bump the version counter.
    pub fn reload(&self, tiers: Vec<Vec<AuthoredSkill>>) {
        let mut by_name: HashMap<String, AuthoredSkill> = HashMap::new();
        for tier in tiers {
            for skill in tier {
                by_name.insert(skill.name.clone(), skill); // later tier wins
            }
        }
        let mut guard = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        *guard = Arc::new(Snapshot { by_name });
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    pub fn len(&self) -> usize {
        self.snap().by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snap().by_name.is_empty()
    }

    fn snap(&self) -> Arc<Snapshot> {
        Arc::clone(&self.snapshot.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Full parsed record for `name`, cloned out of the current snapshot (Unified Chat UI phase
    /// 8, D4) — unlike `fetch_section`'s single-section lazy-load, the editor needs the whole
    /// record (frontmatter + body) to reconstruct a complete file on save/revert.
    pub fn get(&self, name: &str) -> Option<AuthoredSkill> {
        self.snap().by_name.get(name).cloned()
    }

    /// Compact routing table for the L0 system prompt: one `- **name** — when_to_use`
    /// line per skill, name-sorted for determinism. Tag-stripped (an authored file must
    /// not smuggle a live `<tool_call>` into the prompt via its `when_to_use`).
    pub fn routing_table(&self) -> String {
        let snap = self.snap();
        let mut rows: Vec<(&String, &AuthoredSkill)> = snap.by_name.iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        rows.iter()
            .map(|(name, s)| {
                // Strip the NAME too (P2 review MED2): a tag token in a frontmatter `name:`
                // would otherwise reach the L0 prompt unneutralized.
                format!(
                    "- **{}** — {}",
                    strip_tool_tags(name),
                    strip_tool_tags(&s.when_to_use)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Top-`k` playbook BODIES relevant to `task`, ranked by Jaccard word overlap
    /// against each skill's `name + description + when_to_use`. Only playbook-kind
    /// skills (stage-prompt / playbook) are considered — standards are injected
    /// separately by `standards_for`.
    ///
    /// When `domain` is `Some`, only skills whose `domain` matches EXACTLY are returned
    /// — so a finance sub-turn never receives a `developer` playbook. Returns
    /// `(name, body)`; references are NOT included (progressive disclosure).
    ///
    /// `gates` (Pipeline Activation phase 5) excludes any disabled name outright, and lets a
    /// PINNED name bypass the `score > 0.0` Jaccard bar entirely — the user explicitly asked
    /// for it, so it is ranked among the other pinned names and placed ahead of the unpinned
    /// pool, both still bounded by `k`. A default (empty) `gates` reproduces the pre-phase-5
    /// ranking exactly.
    pub fn playbooks_for(
        &self,
        task: &str,
        domain: Option<&str>,
        k: usize,
        gates: &SkillGates,
    ) -> Vec<(String, String)> {
        let snap = self.snap();
        let candidates: Vec<&AuthoredSkill> = snap
            .by_name
            .values()
            .filter(|s| s.kind.is_playbook())
            .filter(|s| match domain {
                Some(d) => s.domain.as_deref() == Some(d),
                None => true,
            })
            .filter(|s| !gates.is_disabled(&s.name))
            .collect();

        let mut pinned: Vec<(f32, &AuthoredSkill)> = candidates
            .iter()
            .copied()
            .filter(|s| gates.is_pinned(&s.name))
            .map(|s| (jaccard_similarity(task, &s.match_text()), s))
            .collect();
        // Highest score first; break ties by name for determinism.
        pinned.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.name.cmp(&b.1.name))
        });

        let mut unpinned: Vec<(f32, &AuthoredSkill)> = candidates
            .iter()
            .copied()
            .filter(|s| !gates.is_pinned(&s.name))
            .map(|s| (jaccard_similarity(task, &s.match_text()), s))
            .filter(|(score, _)| *score > 0.0)
            .collect();
        unpinned.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.name.cmp(&b.1.name))
        });

        pinned
            .into_iter()
            .chain(unpinned)
            .take(k)
            .map(|(_, s)| (s.name.clone(), s.body.clone()))
            .collect()
    }

    /// Standard-kind skill bodies whose `name` is in `names` (e.g. `["lang-rust"]`),
    /// name-sorted. Used by the sub-turn `## Standards` injection after stack detection.
    pub fn standards_for(&self, names: &[&str]) -> Vec<(String, String)> {
        let snap = self.snap();
        let mut out: Vec<(String, String)> = snap
            .by_name
            .values()
            .filter(|s| s.kind == SkillKind::Standard && names.contains(&s.name.as_str()))
            .map(|s| (s.name.clone(), s.body.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Every authored skill's index-level view (phase 11a), name-sorted for a stable
    /// browser ordering. Frontmatter only — no bodies/references (progressive disclosure).
    pub fn list_all(&self) -> Vec<AuthoredSkillInfo> {
        let snap = self.snap();
        let mut out: Vec<AuthoredSkillInfo> = snap
            .by_name
            .values()
            .map(|s| AuthoredSkillInfo {
                name: s.name.clone(),
                description: s.description.clone(),
                when_to_use: s.when_to_use.clone(),
                kind: s.kind.as_str().to_string(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Discovery: skills relevant to `query`, ranked by Jaccard, as `(name, when_to_use)`
    /// — the pair the model needs to decide whether to `skill_fetch` the body.
    pub fn search(&self, query: &str, k: usize) -> Vec<(String, String)> {
        let snap = self.snap();
        let mut scored: Vec<(f32, &AuthoredSkill)> = snap
            .by_name
            .values()
            .map(|s| (jaccard_similarity(query, &s.match_text()), s))
            .filter(|(score, _)| *score > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.name.cmp(&b.1.name))
        });
        scored
            .into_iter()
            .take(k)
            .map(|(_, s)| (s.name.clone(), s.when_to_use.clone()))
            .collect()
    }

    /// Enumerate the fetchable sections of `skill`: the reserved `body` section plus one
    /// entry per reference chunk, as `(section_id, summary)`.
    ///
    /// # Errors
    /// Returns an error (NOT an empty list) when `skill` is unknown — so `skill_fetch`
    /// callers surface a real "no such skill" rather than a misleading empty result.
    pub fn list_sections(&self, skill: &str) -> Result<Vec<(String, String)>> {
        let snap = self.snap();
        let s = snap
            .by_name
            .get(skill)
            .ok_or_else(|| anyhow!("unknown skill '{skill}'"))?;
        let mut out = vec![(BODY_SECTION.to_string(), s.description.clone())];
        out.extend(
            s.references
                .iter()
                .map(|r| (r.section_id.clone(), r.summary.clone())),
        );
        Ok(out)
    }

    /// Fetch exactly ONE section of `skill`: `body` returns the stage-prompt body; any
    /// other id returns that reference chunk's body. This is the runtime-mediated
    /// lazy-load (Claude Code's Read/Skill equivalent) — one chunk, never a dump.
    ///
    /// # Errors
    /// Returns an error when the skill or the section is unknown — an unknown section
    /// must error, never fall back to dumping the whole skill.
    pub fn fetch_section(&self, skill: &str, section: &str) -> Result<String> {
        let snap = self.snap();
        let s = snap
            .by_name
            .get(skill)
            .ok_or_else(|| anyhow!("unknown skill '{skill}'"))?;
        if section == BODY_SECTION {
            return Ok(s.body.clone());
        }
        s.references
            .iter()
            .find(|r| r.section_id == section)
            .map(|r| r.body.clone())
            .ok_or_else(|| anyhow!("unknown section '{section}' for skill '{skill}'"))
    }
}

// ---------------------------------------------------------------------------
// Frontmatter parsing (hand-rolled — controlled format, no YAML dependency)
// ---------------------------------------------------------------------------

/// Split a `---`-delimited document into `(frontmatter, body)`. Tolerant of CRLF.
fn split_frontmatter(raw: &str) -> Result<(String, String)> {
    let normalized = raw.replace("\r\n", "\n");
    let rest = normalized
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow!("missing frontmatter opening '---'"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow!("unterminated frontmatter block"))?;
    let front = rest[..end].to_string();
    // Body starts after the closing '---' line (skip to the next newline after it).
    let after = &rest[end + 1..]; // at the closing '---'
    let body = after
        .find('\n')
        .map(|i| after[i + 1..].to_string())
        .unwrap_or_default();
    Ok((front, body))
}

/// Parse `key: value` scalar lines from a frontmatter block into a map. Values keep
/// their raw text (list parsing happens per-field in `parse_list`). Lines without a
/// colon and blank lines are ignored.
fn parse_scalars(front: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in front.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

/// Parse an inline `[a, b, c]` list (the controlled kit-pack form). A bare non-bracketed
/// value is treated as a single-element list. Empty brackets → empty list.
fn parse_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn skill(name: &str, kind: SkillKind, domain: Option<&str>, blurb: &str) -> AuthoredSkill {
        AuthoredSkill {
            name: name.to_string(),
            description: blurb.to_string(),
            when_to_use: blurb.to_string(),
            domain: domain.map(str::to_string),
            specialists: vec![],
            kind,
            body: format!("BODY OF {name}"),
            references: vec![],
        }
    }

    // ------------------------------------------------------------------
    // Frontmatter parsing
    // ------------------------------------------------------------------

    #[test]
    fn parses_full_frontmatter_and_body() {
        let raw = "---\nname: plan\ndescription: plan things\nwhen_to_use: before coding\ndomain: developer\nkind: stage-prompt\nspecialists: [planner, reviewer]\n---\n# Plan\nbody text here\n";
        let s = AuthoredSkill::from_markdown("fallback", raw).expect("parse");
        assert_eq!(s.name, "plan");
        assert_eq!(s.domain.as_deref(), Some("developer"));
        assert_eq!(s.kind, SkillKind::StagePrompt);
        assert_eq!(s.specialists, vec!["planner", "reviewer"]);
        assert!(s.body.contains("body text here"));
        assert!(!s.body.contains("---"), "frontmatter must not bleed into body");
    }

    #[test]
    fn uses_name_fallback_when_frontmatter_omits_name() {
        let raw = "---\ndescription: d\nwhen_to_use: w\ndomain: developer\nkind: standard\n---\nbody";
        let s = AuthoredSkill::from_markdown("lang-rust", raw).expect("parse");
        assert_eq!(s.name, "lang-rust");
    }

    #[test]
    fn rejects_missing_required_scalar() {
        // No when_to_use.
        let raw = "---\nname: x\ndescription: d\ndomain: developer\nkind: stage-prompt\n---\nbody";
        assert!(AuthoredSkill::from_markdown("x", raw).is_err());
    }

    #[test]
    fn rejects_invalid_kind() {
        let raw = "---\nname: x\ndescription: d\nwhen_to_use: w\ndomain: developer\nkind: bogus\n---\nbody";
        assert!(AuthoredSkill::from_markdown("x", raw).is_err());
    }

    #[test]
    fn validate_complete_flags_missing_domain() {
        // The CI-style validator: a kit-pack skill without a domain must be flagged.
        let raw = "---\nname: x\ndescription: d\nwhen_to_use: w\nkind: stage-prompt\n---\nbody";
        let s = AuthoredSkill::from_markdown("x", raw).expect("parse (domain optional at parse)");
        assert!(s.domain.is_none());
        assert!(
            s.validate_complete().is_err(),
            "missing domain must fail the completeness validator"
        );
    }

    #[test]
    fn to_markdown_round_trips_through_from_markdown() {
        let raw = "---\nname: plan\ndescription: plan things\nwhen_to_use: before coding\ndomain: developer\nkind: stage-prompt\nspecialists: [planner, reviewer]\n---\nbody text here\nsecond line\n";
        let original = AuthoredSkill::from_markdown("fallback", raw).expect("parse original");
        let regenerated = AuthoredSkill::from_markdown("fallback", &original.to_markdown()).expect("parse regenerated");

        assert_eq!(regenerated.name, original.name);
        assert_eq!(regenerated.description, original.description);
        assert_eq!(regenerated.when_to_use, original.when_to_use);
        assert_eq!(regenerated.domain, original.domain);
        assert_eq!(regenerated.kind, original.kind);
        assert_eq!(regenerated.specialists, original.specialists);
        assert_eq!(regenerated.body, original.body);
    }

    #[test]
    fn to_markdown_flattens_embedded_newlines_in_scalars() {
        // A synthesized skill's `description` (promote_to_authored's source) has no
        // single-line guarantee — a stray newline must not corrupt the frontmatter block.
        let mut skill = skill("x", SkillKind::Playbook, None, "line one\nline two");
        skill.when_to_use = "also\nmultiline".to_string();
        let md = skill.to_markdown();
        let reparsed = AuthoredSkill::from_markdown("x", &md).expect("frontmatter must stay parseable");
        assert!(!reparsed.description.contains('\n'));
        assert!(!reparsed.when_to_use.contains('\n'));
    }

    #[test]
    fn tolerates_crlf_line_endings() {
        let raw = "---\r\nname: x\r\ndescription: d\r\nwhen_to_use: w\r\ndomain: developer\r\nkind: standard\r\n---\r\nbody line\r\n";
        let s = AuthoredSkill::from_markdown("x", raw).expect("parse crlf");
        assert_eq!(s.name, "x");
        assert!(s.body.contains("body line"));
    }

    // ------------------------------------------------------------------
    // Registry: precedence, routing, matching, sections
    // ------------------------------------------------------------------

    #[test]
    fn higher_tier_overrides_lower_by_name() {
        let low = skill("plan", SkillKind::StagePrompt, Some("developer"), "kit-pack plan");
        let mut high = skill("plan", SkillKind::StagePrompt, Some("developer"), "project plan");
        high.body = "PROJECT BODY".to_string();
        let reg = AuthoredRegistry::from_tiers(vec![vec![low], vec![high]]);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.fetch_section("plan", BODY_SECTION).unwrap(), "PROJECT BODY");
    }

    #[test]
    fn reload_bumps_version_counter() {
        let reg = AuthoredRegistry::new();
        let v0 = reg.version();
        reg.reload(vec![vec![skill("a", SkillKind::Playbook, None, "x")]]);
        assert!(reg.version() > v0);
    }

    #[test]
    fn routing_table_is_sorted_and_tag_stripped() {
        let reg = AuthoredRegistry::from_tiers(vec![vec![
            skill("zeta", SkillKind::StagePrompt, Some("developer"), "z"),
            {
                let mut s = skill("alpha", SkillKind::StagePrompt, Some("developer"), "a");
                s.when_to_use = "use <tool_call>{}</tool_call> me".to_string();
                s
            },
        ]]);
        let table = reg.routing_table();
        assert!(table.find("alpha").unwrap() < table.find("zeta").unwrap(), "name-sorted");
        assert!(!table.contains("<tool_call>"), "routing table must be tag-stripped");
    }

    #[test]
    fn domain_filter_excludes_other_domains() {
        let reg = AuthoredRegistry::from_tiers(vec![vec![
            skill("fix", SkillKind::StagePrompt, Some("developer"), "fix compile bug error"),
            skill("budget", SkillKind::StagePrompt, Some("finance"), "budget money spend"),
        ]]);
        // A finance sub-turn must never receive the developer playbook.
        let finance = reg.playbooks_for("fix compile bug", Some("finance"), 5, &SkillGates::default());
        assert!(
            finance.iter().all(|(n, _)| n != "fix"),
            "developer playbook leaked into finance domain: {finance:?}"
        );
    }

    #[test]
    fn jaccard_ranks_relevant_playbook_over_irrelevant() {
        // "sửa bug compile" must match fix (has 'bug'/'compile') over a writer playbook.
        let reg = AuthoredRegistry::from_tiers(vec![vec![
            skill("fix", SkillKind::StagePrompt, Some("developer"), "fix compile bug error root cause"),
            skill("writer", SkillKind::Playbook, Some("developer"), "write essays prose narrative story"),
        ]]);
        let hits = reg.playbooks_for("sửa bug compile", Some("developer"), 1, &SkillGates::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "fix", "the compile/bug task must rank fix first");
    }

    // ------------------------------------------------------------------
    // Pipeline Activation phase 5 — skill enable/pin gate enforcement
    // ------------------------------------------------------------------

    #[test]
    fn disabled_skill_is_excluded_even_when_it_would_otherwise_rank_first() {
        let reg = AuthoredRegistry::from_tiers(vec![vec![
            skill("fix", SkillKind::StagePrompt, Some("developer"), "fix compile bug error root cause"),
            skill("writer", SkillKind::Playbook, Some("developer"), "write essays prose narrative story"),
        ]]);
        let gates = SkillGates::new(HashSet::from(["fix".to_string()]), HashSet::new());
        let hits = reg.playbooks_for("sửa bug compile", Some("developer"), 5, &gates);
        assert!(
            hits.iter().all(|(n, _)| n != "fix"),
            "disabled skill must never reach the injected pool: {hits:?}"
        );
    }

    #[test]
    fn pinned_skill_is_ordered_first_and_bypasses_the_score_floor() {
        // "writer" scores 0 against this task (no word overlap) and would normally be
        // filtered out entirely; pinning it must surface it anyway, ahead of the
        // genuinely-matching "fix" skill.
        let reg = AuthoredRegistry::from_tiers(vec![vec![
            skill("fix", SkillKind::StagePrompt, Some("developer"), "fix compile bug error root cause"),
            skill("writer", SkillKind::Playbook, Some("developer"), "write essays prose narrative story"),
        ]]);
        let gates = SkillGates::new(HashSet::new(), HashSet::from(["writer".to_string()]));
        let hits = reg.playbooks_for("sửa bug compile", Some("developer"), 5, &gates);
        assert_eq!(
            hits.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["writer", "fix"],
            "pinned skill must be ordered first even though it does not match the task: {hits:?}"
        );
    }

    #[test]
    fn pinned_skill_stays_bounded_by_the_k_budget() {
        let reg = AuthoredRegistry::from_tiers(vec![vec![
            skill("fix", SkillKind::StagePrompt, Some("developer"), "fix compile bug error root cause"),
            skill("writer", SkillKind::Playbook, Some("developer"), "write essays prose narrative story"),
        ]]);
        let gates = SkillGates::new(HashSet::new(), HashSet::from(["writer".to_string()]));
        // k=1: the pinned skill takes the single slot, the otherwise-matching "fix" is dropped.
        let hits = reg.playbooks_for("sửa bug compile", Some("developer"), 1, &gates);
        assert_eq!(hits.len(), 1, "pinned entries stay bounded by the k budget: {hits:?}");
        assert_eq!(hits[0].0, "writer");
    }

    #[test]
    fn default_gates_reproduce_unset_state_exactly() {
        // Regression guard (phase 5 Risk Assessment): default (empty) gates must yield the
        // identical ranking `jaccard_ranks_relevant_playbook_over_irrelevant` already pins.
        let reg = AuthoredRegistry::from_tiers(vec![vec![
            skill("fix", SkillKind::StagePrompt, Some("developer"), "fix compile bug error root cause"),
            skill("writer", SkillKind::Playbook, Some("developer"), "write essays prose narrative story"),
        ]]);
        let hits = reg.playbooks_for("sửa bug compile", Some("developer"), 1, &SkillGates::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "fix");
    }

    #[test]
    fn playbooks_return_body_not_references() {
        // NO-LOAD-ALL: a matched playbook injects only its body — reference chunk text
        // must NOT appear until fetch_section pulls it.
        let mut s = skill("cook", SkillKind::StagePrompt, Some("developer"), "cook build implement code");
        s.references.push(ReferenceChunk {
            section_id: "tdd-workflow".to_string(),
            summary: "tests first".to_string(),
            body: "SECRET_REFERENCE_BODY".to_string(),
        });
        let reg = AuthoredRegistry::from_tiers(vec![vec![s]]);
        let hits = reg.playbooks_for("cook build implement", Some("developer"), 1, &SkillGates::default());
        assert_eq!(hits.len(), 1);
        assert!(
            !hits[0].1.contains("SECRET_REFERENCE_BODY"),
            "reference body must stay unloaded until fetch_section pulls it"
        );
    }

    #[test]
    fn fetch_section_returns_one_chunk_and_errors_on_unknown() {
        let mut s = skill("cook", SkillKind::StagePrompt, Some("developer"), "cook");
        s.references.push(ReferenceChunk {
            section_id: "tdd-workflow".to_string(),
            summary: "tests first".to_string(),
            body: "REF BODY".to_string(),
        });
        let reg = AuthoredRegistry::from_tiers(vec![vec![s]]);

        assert_eq!(reg.fetch_section("cook", "tdd-workflow").unwrap(), "REF BODY");
        assert!(reg.fetch_section("cook", BODY_SECTION).unwrap().contains("BODY OF cook"));
        // Unknown section → error, NOT a full dump.
        assert!(reg.fetch_section("cook", "does-not-exist").is_err());
        // Unknown skill → error.
        assert!(reg.fetch_section("nope", BODY_SECTION).is_err());
        assert!(reg.list_sections("nope").is_err());
    }

    #[test]
    fn list_sections_enumerates_body_plus_references() {
        let mut s = skill("cook", SkillKind::StagePrompt, Some("developer"), "cook");
        s.references.push(ReferenceChunk {
            section_id: "tdd-workflow".to_string(),
            summary: "tests first".to_string(),
            body: "b".to_string(),
        });
        let reg = AuthoredRegistry::from_tiers(vec![vec![s]]);
        let sections = reg.list_sections("cook").unwrap();
        assert_eq!(sections[0].0, BODY_SECTION);
        assert!(sections.iter().any(|(id, _)| id == "tdd-workflow"));
    }

    #[test]
    fn standards_for_filters_by_name_and_kind() {
        let reg = AuthoredRegistry::from_tiers(vec![vec![
            skill("lang-rust", SkillKind::Standard, Some("developer"), "rust"),
            skill("lang-python", SkillKind::Standard, Some("developer"), "python"),
            skill("cook", SkillKind::StagePrompt, Some("developer"), "cook"),
        ]]);
        let got = reg.standards_for(&["lang-rust"]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "lang-rust");
    }
}
