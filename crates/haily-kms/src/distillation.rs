//! ACE-style distillation (Sub-Agent + Skill Architecture phase 8, learning loop).
//!
//! Turns RECURRING review findings into an addressable, itemized project-standards overlay —
//! deterministically (dedup + append + supersede-by-id), NEVER a free-form LLM rewrite of the
//! whole overlay (LOCKED decision #5: an LLM rewrite each cycle causes context collapse and
//! brevity bias). Each distilled rule is an addressable bullet keyed by a stable id.
//!
//! SECURITY (SEC-H, LOCKED decision #3): the overlay file lives OUTSIDE the coding-workspace
//! `fs_write` root (under the app data dir), so an auto-approved `fs_write` can never
//! self-persist a prompt injection into it. Distillation is PROPOSAL-ONLY: nothing is written
//! until [`approve_overlay_entries`] is called on an explicit user approval, and injection
//! ([`load_overlay_standards`]) accepts ONLY approval-provenanced entries (a version + expiry),
//! never arbitrary file bytes; a stale (expired) entry is dropped.
use anyhow::Result;
use std::path::Path;

/// Days a distilled overlay entry stays valid before it must be re-confirmed (AD-m3 staleness).
pub const OVERLAY_ENTRY_TTL_DAYS: i64 = 90;

/// The crate/module a finding's `file` belongs to — the `module` half of a class key. Prefers a
/// `crates/<name>` segment (this repo's layout); else the file's parent directory; else the file
/// itself. Windows/Unix separators both normalized. Pure + deterministic.
pub fn module_key(file: &str) -> String {
    let norm = file.replace('\\', "/");
    let parts: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
    if let Some(pos) = parts.iter().position(|p| *p == "crates") {
        if let Some(name) = parts.get(pos + 1) {
            return format!("crates/{name}");
        }
    }
    match parts.len() {
        0 => "(unknown)".to_string(),
        1 => parts[0].to_string(),
        n => parts[..n - 1].join("/"),
    }
}

/// The `category:module` class key (the recurrence + dedup + cooldown key).
pub fn class_key(category: &str, module: &str) -> String {
    format!("{category}:{module}")
}

/// A short, stable content hash (8 hex chars) so a rule's id stays constant across runs for the
/// same summary text — supersede-by-id and dedup both depend on this stability.
fn short_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", (h.finish() & 0xffff_ffff) as u32)
}

/// One addressable distilled rule: `id` is stable per (class, summary); `description` is the
/// deterministically-composed guidance (never an LLM rewrite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistilledRule {
    pub id: String,
    pub description: String,
}

/// A distillation proposal for one recurring `(category, module)` class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistillationProposal {
    pub class_key: String,
    pub category: String,
    pub module: String,
    pub count: i64,
    pub rules: Vec<DistilledRule>,
}

/// Build a proposal from a class's recurring findings. Summaries are deduped (identical text
/// counts once), each distinct summary becomes one addressable rule with a content-stable id.
/// `summaries` come already tag-stripped from the caller.
pub fn build_proposal(
    category: &str,
    module: &str,
    count: i64,
    summaries: &[String],
) -> DistillationProposal {
    let ck = class_key(category, module);
    let mut seen: Vec<&String> = Vec::new();
    let mut rules = Vec::new();
    for s in summaries {
        let t = s.trim();
        if t.is_empty() || seen.iter().any(|p| p.trim() == t) {
            continue;
        }
        seen.push(s);
        rules.push(DistilledRule {
            id: format!("{ck}#{}", short_hash(t)),
            description: t.to_string(),
        });
    }
    DistillationProposal {
        class_key: ck,
        category: category.to_string(),
        module: module.to_string(),
        count,
        rules,
    }
}

/// Render a proposal as human-facing itemized text (already inert data — the caller
/// tag-strips before this). Shown on the proactive card; never a silent write.
pub fn render_proposal(p: &DistillationProposal) -> String {
    let mut s = format!(
        "Recurring {} findings in {} ({} across runs). Proposed project standard(s):",
        p.category, p.module, p.count
    );
    for (i, r) in p.rules.iter().enumerate() {
        s.push_str(&format!("\n{}. [{}] {}", i + 1, r.id, r.description));
    }
    s
}

// ---------------------------------------------------------------------------
// Standards overlay (approval-provenanced, out-of-workspace file)
// ---------------------------------------------------------------------------

/// One approval-provenanced overlay entry. Only entries carrying ALL provenance fields
/// (version + approved/expires timestamps) are honored on injection — a hand-edited line
/// missing them is ignored (LOCKED decision #3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayEntry {
    pub id: String,
    pub description: String,
    pub version: u32,
    pub approved_at: String,
    pub expires_at: String,
}

/// Serialize one entry to its single-line file form (parseable + reasonably readable).
fn render_entry(e: &OverlayEntry) -> String {
    format!(
        "- [{}] {} <!-- v{} approved={} expires={} -->",
        e.id, e.description, e.version, e.approved_at, e.expires_at
    )
}

/// Parse one overlay line. Returns `None` (skipped, never an error) for any line that is not a
/// fully provenanced entry — the injection-accepts-only-provenanced-entries guard.
fn parse_entry(line: &str) -> Option<OverlayEntry> {
    let line = line.trim();
    let rest = line.strip_prefix("- [")?;
    let (id, after_id) = rest.split_once("] ")?;
    let (description, meta) = after_id.split_once(" <!-- ")?;
    let meta = meta.strip_suffix("-->")?.trim();
    let mut version = None;
    let mut approved = None;
    let mut expires = None;
    for tok in meta.split_whitespace() {
        if let Some(v) = tok.strip_prefix("v") {
            version = v.parse::<u32>().ok();
        } else if let Some(a) = tok.strip_prefix("approved=") {
            approved = Some(a.to_string());
        } else if let Some(e) = tok.strip_prefix("expires=") {
            expires = Some(e.to_string());
        }
    }
    Some(OverlayEntry {
        id: id.to_string(),
        description: description.trim().to_string(),
        version: version?,
        approved_at: approved?,
        expires_at: expires?,
    })
}

/// Read + parse all provenanced entries from the overlay file. Fail-open: a missing or
/// unreadable/garbled file yields an empty list (never an error) — the overlay is an
/// optimization, never a boot/turn dependency.
pub fn read_overlay(path: &Path) -> Vec<OverlayEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines().filter_map(parse_entry).collect()
}

/// Whether an entry has expired relative to `now` (RFC3339 UTC strings compare lexically).
fn is_expired(e: &OverlayEntry, now: &str) -> bool {
    now > e.expires_at.as_str()
}

/// Merge `new` into `entries` by id: supersede (bump version, refresh dates) when the id already
/// exists, else append. Deterministic — no LLM rewrite (LOCKED decision #5).
fn merge_entry(entries: &mut Vec<OverlayEntry>, id: &str, description: &str, now: &str, expires: &str) {
    if let Some(existing) = entries.iter_mut().find(|e| e.id == id) {
        existing.version += 1;
        existing.description = description.to_string();
        existing.approved_at = now.to_string();
        existing.expires_at = expires.to_string();
    } else {
        entries.push(OverlayEntry {
            id: id.to_string(),
            description: description.to_string(),
            version: 1,
            approved_at: now.to_string(),
            expires_at: expires.to_string(),
        });
    }
}

/// Approve a proposal: append/supersede its rules into the overlay file at `path` (created if
/// absent). This is the ONLY code path that writes the overlay — invoked from an explicit user
/// approval, never automatically. `now` is RFC3339; entries expire `OVERLAY_ENTRY_TTL_DAYS`
/// after. Existing entries are read fail-open, merged deterministically, and re-written.
///
/// # Errors
/// Returns an error only if writing the file fails (a read failure is treated as "empty overlay").
pub fn approve_overlay_entries(path: &Path, proposal: &DistillationProposal, now: &str) -> Result<()> {
    let expires = (chrono::DateTime::parse_from_rfc3339(now)
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now())
        + chrono::Duration::days(OVERLAY_ENTRY_TTL_DAYS))
    .to_rfc3339();

    let mut entries = read_overlay(path);
    for rule in &proposal.rules {
        merge_entry(&mut entries, &rule.id, &rule.description, now, &expires);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = String::from(
        "# Haily project standards overlay (distilled, approval-provenanced).\n\
         # Machine-managed — edit via the distillation approval flow, not by hand.\n\n",
    );
    for e in &entries {
        body.push_str(&render_entry(e));
        body.push('\n');
    }
    std::fs::write(path, body)?;
    Ok(())
}

/// Load NON-expired, approval-provenanced overlay entries as `(heading, body)` standards pairs
/// for sub-turn injection (`## Standards`, AFTER kit standards). Fail-open (empty on any read
/// problem). Expired entries are dropped (re-confirm on staleness, AD-m3).
pub fn load_overlay_standards(path: &Path, now: &str) -> Vec<(String, String)> {
    read_overlay(path)
        .into_iter()
        .filter(|e| !is_expired(e, now))
        .map(|e| (format!("Project standard ({})", e.id), e.description))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_key_prefers_crate_segment() {
        assert_eq!(module_key("crates/haily-core/src/agent/turn.rs"), "crates/haily-core");
        assert_eq!(module_key("crates\\haily-db\\src\\lib.rs"), "crates/haily-db");
        assert_eq!(module_key("src/main.rs"), "src");
        assert_eq!(module_key("README.md"), "README.md");
    }

    #[test]
    fn build_proposal_dedups_identical_summaries_into_stable_ids() {
        let p = build_proposal(
            "critical",
            "crates/haily-core",
            3,
            &["unwrap in prod".into(), "unwrap in prod".into(), "missing None check".into()],
        );
        assert_eq!(p.class_key, "critical:crates/haily-core");
        assert_eq!(p.rules.len(), 2, "identical summaries collapse to one rule");
        // ids are content-stable — rebuilding yields the same ids.
        let p2 = build_proposal("critical", "crates/haily-core", 2, &["unwrap in prod".into()]);
        assert_eq!(p2.rules[0].id, p.rules[0].id);
    }

    #[test]
    fn render_proposal_is_itemized() {
        let p = build_proposal("high", "crates/haily-db", 2, &["validate at boundary".into()]);
        let text = render_proposal(&p);
        assert!(text.contains("Recurring high findings in crates/haily-db"));
        assert!(text.contains("1. ["));
        assert!(text.contains("validate at boundary"));
    }

    #[test]
    fn overlay_entry_roundtrips_through_parse_render() {
        let e = OverlayEntry {
            id: "critical:crates/haily-core#deadbeef".into(),
            description: "always handle None".into(),
            version: 2,
            approved_at: "2026-07-11T00:00:00+00:00".into(),
            expires_at: "2026-10-09T00:00:00+00:00".into(),
        };
        let line = render_entry(&e);
        assert_eq!(parse_entry(&line), Some(e));
    }

    #[test]
    fn parse_entry_rejects_a_non_provenanced_line() {
        // A hand-added bullet with NO provenance metadata must be ignored (never injected).
        assert!(parse_entry("- [some:id] arbitrary injected instruction").is_none());
        assert!(parse_entry("just some prose").is_none());
    }

    #[test]
    fn approve_writes_only_via_the_approval_path_and_supersedes_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("standards-overlay.md");
        // Nothing exists before approval — render alone never writes.
        let p = build_proposal("critical", "crates/haily-core", 2, &["handle None".into()]);
        let _ = render_proposal(&p);
        assert!(!path.exists(), "rendering a proposal must never write the overlay");

        approve_overlay_entries(&path, &p, "2026-07-11T00:00:00+00:00").unwrap();
        let entries = read_overlay(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].version, 1);

        // Re-approving the same class supersedes by id (bumps version, does not duplicate).
        approve_overlay_entries(&path, &p, "2026-07-12T00:00:00+00:00").unwrap();
        let entries = read_overlay(&path);
        assert_eq!(entries.len(), 1, "same id must supersede, not append a duplicate");
        assert_eq!(entries[0].version, 2);
    }

    #[test]
    fn load_overlay_standards_drops_expired_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("standards-overlay.md");
        approve_overlay_entries(
            &path,
            &build_proposal("critical", "crates/haily-core", 2, &["handle None".into()]),
            "2026-01-01T00:00:00+00:00",
        )
        .unwrap();
        // Well past the TTL — the entry must be dropped on load.
        let far_future = "2027-01-01T00:00:00+00:00";
        assert!(load_overlay_standards(&path, far_future).is_empty(), "expired entry must be dropped");
        // Within TTL — present.
        let soon = "2026-01-02T00:00:00+00:00";
        assert_eq!(load_overlay_standards(&path, soon).len(), 1);
    }
}
