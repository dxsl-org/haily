//! Structured skill editor (Unified Chat UI phase 8, D4): a 4-field model
//! (Procedure / Success conditions / Forbidden actions / Required from user) that maps to
//! markdown sections, shared by BOTH authored (kit-pack file, sha256-pinned) and synthesized
//! (`kms_skills` row) skills. One version-history table
//! (`haily_db::queries::skill_versions`) covers both kinds; reverting re-applies the atomic
//! edit path for whichever kind the version belongs to.
//!
//! Submodules: `markdown` (pure render/parse, unit-testable without a DB or filesystem),
//! `guard` (traversal-safe skill-name validation), `ops` (the async edit/revert/promote/archive
//! orchestration the Tauri commands call).

mod guard;
mod markdown;
mod ops;

pub use guard::validate_skill_name;
pub use markdown::{parse_markdown, render_markdown};
pub use ops::{
    archive_synthesized, edit_skill, get_skill_detail, list_versions, promote_to_authored, revert_skill,
};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Cap on any single structured field (Security Considerations: "cap content size") — well
/// beyond what a hand-authored playbook needs, but small enough that a runaway
/// Draft-with-Haily generation cannot balloon a kit-pack file.
pub const MAX_FIELD_BYTES: usize = 20_000;

/// The 4-field structured shape both the authored and synthesized edit paths render to/from
/// markdown. Shared with the GUI (P09) as the wire type for the editor form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDraft {
    pub procedure: String,
    pub success_conditions: String,
    pub forbidden_actions: String,
    pub required_from_user: String,
}

/// Which store a skill's content lives in — drives which atomic edit path `ops` dispatches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillEditKind {
    /// kit-pack markdown file, sha256-pinned in `manifest.json`.
    Authored,
    /// `kms_skills` row, EMA-confidence/decay lifecycle.
    Synthesized,
}

impl SkillEditKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillEditKind::Authored => "authored",
            SkillEditKind::Synthesized => "synthesized",
        }
    }

    /// # Errors
    /// Returns an error for any value other than `"authored"`/`"synthesized"`.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "authored" => Ok(SkillEditKind::Authored),
            "synthesized" => Ok(SkillEditKind::Synthesized),
            other => bail!("unknown skill kind '{other}' (expected 'authored' or 'synthesized')"),
        }
    }
}

/// One skill's editable view, for the editor's "open" action — the current live content mapped
/// into the 4-field draft.
#[derive(Debug, Clone, Serialize)]
pub struct SkillDetail {
    pub name: String,
    pub kind: String,
    pub draft: SkillDraft,
}
