//! Traversal-safe skill-name validation (Unified Chat UI phase 8, D4): a skill name becomes a
//! bare filename component (`skills/<name>.md`), so it must never be able to escape the
//! kit-pack directory.

use anyhow::{bail, Result};

const MAX_NAME_LEN: usize = 64;

/// Names Windows treats as reserved DEVICE files regardless of extension (`CON.md` still opens
/// the console device) — checked case-insensitively.
const RESERVED_WINDOWS_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Validate a skill name is safe to use as a bare filename component. An ALLOWLIST
/// (letters/digits/hyphen/underscore) is used rather than a traversal blocklist — every
/// escaping character (`/`, `\`, `..`, a drive letter, NUL, …) is rejected by construction
/// instead of enumerated one at a time.
///
/// # Errors
/// Returns an error naming the violation for an empty, over-long, disallowed-character, or
/// Windows-reserved-device name.
pub fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("skill name must not be empty");
    }
    if name.len() > MAX_NAME_LEN {
        bail!("skill name too long (max {MAX_NAME_LEN} chars)");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("skill name '{name}' may only contain letters, digits, '-' and '_'");
    }
    if RESERVED_WINDOWS_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
        bail!("skill name '{name}' is a reserved device name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_kit_pack_style_names() {
        for name in ["plan", "cook", "lang-rust", "design_lens_risk", "a1"] {
            assert!(validate_skill_name(name).is_ok(), "'{name}' should be valid");
        }
    }

    #[test]
    fn rejects_traversal_and_separators() {
        for name in ["../etc/passwd", "..", "a/b", "a\\b", "/etc/passwd", "C:\\evil"] {
            assert!(validate_skill_name(name).is_err(), "'{name}' must be rejected");
        }
    }

    #[test]
    fn rejects_empty_overlong_and_reserved_names() {
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name(&"a".repeat(65)).is_err());
        assert!(validate_skill_name("CON").is_err());
        assert!(validate_skill_name("com1").is_err());
        assert!(validate_skill_name("nul").is_err());
    }
}
