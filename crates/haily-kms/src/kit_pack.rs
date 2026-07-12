//! Kit-pack loader: reads a versioned, sha256-manifest-verified export of authored
//! skills/standards from disk into [`AuthoredSkill`]s.
//!
//! # Integrity boundary (LOCKED — phase-02 Decision 4)
//! `manifest.json` lists every shipped file with its sha256. At load, each file's bytes
//! are re-hashed and compared; on mismatch the file is SKIPPED (+warn) and boot
//! continues — a tampered/rotted file drops one skill, it never fails startup. A
//! detached cryptographic signature (authenticity vs a co-edited manifest) is a deferred
//! follow-up (needs an out-of-tree key). The loader NEVER executes anything from the
//! kit-pack; it only parses text.
//!
//! # Layout
//! ```text
//! <kit-pack>/
//!   manifest.json
//!   skills/<name>.md          -> AuthoredSkill (stage-prompt | playbook)
//!   standards/<name>.md       -> AuthoredSkill (standard)
//!   references/<skill>/<sec>.md -> ReferenceChunk attached to <skill>
//! ```

use crate::authored_skills::{AuthoredSkill, ReferenceChunk};
use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path};

/// Max bytes for a single kit-pack file (P2 review LOW2) — bounds worst-case load memory.
const MAX_KIT_FILE_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB

/// True if a manifest file key is absolute or contains a `..`/root/prefix component, i.e. it
/// could resolve OUTSIDE the kit-pack dir. Such entries are rejected before any read (P2 review
/// LOW1) so an edited manifest cannot turn integrity-loading into arbitrary-file reads.
fn is_escaping_rel(rel: &str) -> bool {
    let p = Path::new(rel);
    p.is_absolute()
        || p.components().any(|c| {
            matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        })
}

/// The kit-pack manifest — version metadata + per-file sha256 (hex).
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// The HailyKit commit this pack was exported from (`"unknown"` if unavailable).
    #[serde(default)]
    pub source_commit: String,
    /// Relative path (forward-slashed) → sha256 hex of the file's bytes.
    pub files: BTreeMap<String, String>,
}

/// Load and verify a kit-pack directory into a flat list of authored skills (references
/// already attached). The returned list is ONE precedence tier (the kit-pack tier).
///
/// # Errors
/// Returns an error only when the manifest itself is missing or unparseable — an
/// individual file that fails verification is skipped (logged), not fatal.
pub fn load(dir: &Path) -> Result<Vec<AuthoredSkill>> {
    let manifest_path = dir.join("manifest.json");
    let manifest_raw = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading kit-pack manifest {}", manifest_path.display()))?;
    let manifest: Manifest =
        serde_json::from_str(&manifest_raw).context("parsing kit-pack manifest.json")?;

    // Verify every listed file; keep only those whose bytes match the pinned hash.
    let mut verified: HashMap<String, String> = HashMap::new();
    for (rel, expected) in &manifest.files {
        // Path-escape guard (P2 review LOW1): a manifest key like `../../secret.md` (or an
        // absolute path) must not read a file outside the kit-pack dir. Since the manifest is
        // only sha256-pinned (signature deferred), an attacker who edits it could otherwise turn
        // "load tampered pack content" into "read an arbitrary file off disk".
        if is_escaping_rel(rel) {
            tracing::warn!(file = %rel, "kit-pack manifest entry escapes the pack dir — skipping");
            continue;
        }
        let path = dir.join(rel);
        // Size cap (P2 review LOW2): bound worst-case memory — skip an oversized entry before
        // reading it whole.
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > MAX_KIT_FILE_BYTES {
                tracing::warn!(file = %rel, size = meta.len(), "kit-pack file exceeds size cap — skipping");
                continue;
            }
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(file = %rel, "kit-pack file unreadable — skipping: {e}");
                continue;
            }
        };
        let actual = sha256_hex(&bytes);
        if &actual != expected {
            tracing::warn!(
                file = %rel,
                "kit-pack file sha256 mismatch (altered out-of-band) — skipping"
            );
            continue;
        }
        match String::from_utf8(bytes) {
            Ok(text) => {
                verified.insert(rel.clone(), text);
            }
            Err(_) => tracing::warn!(file = %rel, "kit-pack file is not valid UTF-8 — skipping"),
        }
    }

    Ok(build_skills(&verified, &manifest))
}

/// Assemble verified file contents into skills, attaching reference chunks by skill name.
fn build_skills(verified: &HashMap<String, String>, manifest: &Manifest) -> Vec<AuthoredSkill> {
    // 1) Collect reference chunks: references/<skill>/<section>.md
    let mut refs_by_skill: HashMap<String, Vec<ReferenceChunk>> = HashMap::new();
    for (rel, content) in verified {
        let parts: Vec<&str> = rel.split('/').collect();
        if parts.first() == Some(&"references") && parts.len() == 3 {
            let skill = parts[1].to_string();
            let section_id = parts[2].trim_end_matches(".md").to_string();
            refs_by_skill.entry(skill).or_default().push(ReferenceChunk {
                section_id,
                summary: first_line_summary(content),
                body: content.trim().to_string(),
            });
        }
    }
    for chunks in refs_by_skill.values_mut() {
        chunks.sort_by(|a, b| a.section_id.cmp(&b.section_id));
    }

    // 2) Parse skills/ and standards/ files, then attach their references.
    let mut skills = Vec::new();
    for (rel, content) in verified {
        let parts: Vec<&str> = rel.split('/').collect();
        let is_skill = parts.first() == Some(&"skills") && parts.len() == 2;
        let is_standard = parts.first() == Some(&"standards") && parts.len() == 2;
        if !(is_skill || is_standard) {
            continue;
        }
        let stem = parts[1].trim_end_matches(".md");
        match AuthoredSkill::from_markdown(stem, content) {
            Ok(mut skill) => {
                if let Some(chunks) = refs_by_skill.remove(&skill.name) {
                    skill.references = chunks;
                }
                skills.push(skill);
            }
            Err(e) => tracing::warn!(file = %rel, "skipping unparseable kit-pack skill: {e:#}"),
        }
    }

    tracing::info!(
        version = %manifest.version,
        source_commit = %manifest.source_commit,
        count = skills.len(),
        "kit-pack parsed"
    );
    skills
}

/// First non-empty line of a reference file, stripped of a leading markdown heading —
/// used as the one-line summary `skill_list_sections` shows for the chunk.
fn first_line_summary(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .unwrap_or_default()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Write a minimal 2-file kit-pack (one skill + one reference) with a correct
    /// manifest into a temp dir; returns the dir. `tamper` mutates the skill file AFTER
    /// the manifest is written, so its hash no longer matches.
    fn make_pack(tamper: bool) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let skill_body = "---\nname: cook\ndescription: build code\nwhen_to_use: implement a change\ndomain: developer\nkind: stage-prompt\n---\ncook body\n";
        let ref_body = "# TDD\ntests first\n";
        fs::create_dir_all(dir.path().join("skills")).unwrap();
        fs::create_dir_all(dir.path().join("references/cook")).unwrap();
        fs::write(dir.path().join("skills/cook.md"), skill_body).unwrap();
        fs::write(dir.path().join("references/cook/tdd.md"), ref_body).unwrap();

        let mut files = BTreeMap::new();
        files.insert("skills/cook.md".to_string(), sha256_hex(skill_body.as_bytes()));
        files.insert(
            "references/cook/tdd.md".to_string(),
            sha256_hex(ref_body.as_bytes()),
        );
        let manifest = format!(
            "{{\"version\":\"1\",\"source_commit\":\"test\",\"files\":{}}}",
            serde_json::to_string(&files).unwrap()
        );
        fs::write(dir.path().join("manifest.json"), manifest).unwrap();

        if tamper {
            // Alter the skill file's bytes after the manifest was pinned.
            fs::write(dir.path().join("skills/cook.md"), format!("{skill_body}TAMPERED")).unwrap();
        }
        dir
    }

    #[test]
    fn loads_verified_pack_with_references_attached() {
        let dir = make_pack(false);
        let skills = load(dir.path()).expect("load");
        assert_eq!(skills.len(), 1);
        let cook = &skills[0];
        assert_eq!(cook.name, "cook");
        assert_eq!(cook.references.len(), 1, "reference chunk must be attached");
        assert_eq!(cook.references[0].section_id, "tdd");
        assert!(!cook.body.contains("tests first"), "reference must not be in body");
    }

    #[test]
    fn tampered_file_is_skipped_and_load_still_succeeds() {
        // CRITICAL scenario: a sha256 mismatch drops the skill, boot continues.
        let dir = make_pack(true);
        let skills = load(dir.path()).expect("load must not fail on a tampered file");
        assert!(
            skills.is_empty(),
            "the tampered skill must be skipped, leaving no skills — got {skills:?}"
        );
    }

    #[test]
    fn missing_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_err(), "absent manifest is a load error the caller tolerates");
    }

    /// CI-style frontmatter validator over the SHIPPED kit-pack: every file listed in
    /// the manifest must verify against its sha256 (no drift between files and manifest),
    /// and every parsed skill must pass `validate_complete` (name + domain present). This
    /// is the "frontmatter validator green over shipped kit-pack" success criterion.
    #[test]
    fn shipped_kit_pack_is_valid() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/kit-pack");
        if !root.join("manifest.json").is_file() {
            // Not present in this checkout — nothing to validate.
            return;
        }
        // 1) Every manifest file verifies (a mismatch would drop it from `load`, which we
        //    detect by comparing the loaded count to the number of skill/standard files).
        let manifest: super::Manifest =
            serde_json::from_str(&fs::read_to_string(root.join("manifest.json")).unwrap()).unwrap();
        let mut declared_skill_files = 0usize;
        for rel in manifest.files.keys() {
            let parts: Vec<&str> = rel.split('/').collect();
            if (parts.first() == Some(&"skills") || parts.first() == Some(&"standards"))
                && parts.len() == 2
            {
                declared_skill_files += 1;
            }
            // Verify the hash so a drifted file fails CI here, not silently at boot.
            let bytes = fs::read(root.join(rel)).expect("manifest file must exist");
            assert_eq!(
                &sha256_hex(&bytes),
                manifest.files.get(rel).unwrap(),
                "sha256 drift for {rel} — regenerate manifest.json"
            );
        }

        let skills = load(&root).expect("shipped kit-pack must load");
        assert_eq!(
            skills.len(),
            declared_skill_files,
            "every shipped skill/standard file must parse (none skipped)"
        );
        for skill in &skills {
            skill
                .validate_complete()
                .unwrap_or_else(|e| panic!("shipped skill failed frontmatter validation: {e:#}"));
        }
        // The curated coding core must be present.
        for expected in ["plan", "cook", "review", "test", "fix", "scout", "lang-rust"] {
            assert!(
                skills.iter().any(|s| s.name == expected),
                "shipped kit-pack missing curated skill '{expected}'"
            );
        }
    }

    #[test]
    fn escaping_manifest_paths_are_rejected() {
        assert!(is_escaping_rel("../../secret.md"));
        assert!(is_escaping_rel("skills/../../etc/passwd"));
        #[cfg(windows)]
        assert!(is_escaping_rel("C:\\Windows\\win.ini"));
        #[cfg(not(windows))]
        assert!(is_escaping_rel("/etc/passwd"));
        assert!(!is_escaping_rel("skills/plan.md"));
        assert!(!is_escaping_rel("references/cook/tdd-workflow.md"));
    }
}
