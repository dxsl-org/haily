//! Stack detection for standards injection: inspects a directory for build-manifest
//! marker files (`Cargo.toml` / `package.json` / `pyproject.toml` …) and maps each hit
//! to an authored `lang-*` standard name.
//!
//! Works standalone from any directory (no `CodingWorkspace` required) — the sub-turn
//! path uses the CWD fallback today, while the pipeline engine (P4) will detect against
//! a real workspace root and pre-inject the matching standard deterministically.

use std::path::Path;

/// A detected language/tooling stack. Only the stacks with a shipped standard in the
/// kit-pack are represented (YAGNI — add more as standards are curated).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stack {
    Rust,
    TypeScript,
    Python,
}

impl Stack {
    /// The authored standard's `name` (frontmatter) this stack maps to.
    pub fn standard_name(&self) -> &'static str {
        match self {
            Stack::Rust => "lang-rust",
            Stack::TypeScript => "lang-typescript",
            Stack::Python => "lang-python",
        }
    }
}

/// Detect the stacks present directly under `dir` from their marker files. Order is
/// stable (Rust, TypeScript, Python) so downstream rendering is deterministic. A repo
/// with several manifests (polyglot) returns several stacks.
pub fn detect_stacks(dir: &Path) -> Vec<Stack> {
    let mut out = Vec::new();
    if dir.join("Cargo.toml").is_file() {
        out.push(Stack::Rust);
    }
    if dir.join("package.json").is_file() || dir.join("tsconfig.json").is_file() {
        out.push(Stack::TypeScript);
    }
    if dir.join("pyproject.toml").is_file()
        || dir.join("setup.py").is_file()
        || dir.join("requirements.txt").is_file()
    {
        out.push(Stack::Python);
    }
    out
}

/// Standard names for the stacks detected under `dir`.
pub fn detect_standard_names_in(dir: &Path) -> Vec<String> {
    detect_stacks(dir)
        .iter()
        .map(|s| s.standard_name().to_string())
        .collect()
}

/// Standard names for the current working directory. Best-effort: returns an empty list
/// on any error rather than propagating — a missing standard is never worth failing a
/// turn over (the deterministic, workspace-rooted path is P4's job).
pub fn detect_standard_names() -> Vec<String> {
    std::env::current_dir()
        .map(|d| detect_standard_names_in(&d))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detects_rust_from_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect_standard_names_in(dir.path()), vec!["lang-rust"]);
    }

    #[test]
    fn detects_typescript_from_package_json() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect_standard_names_in(dir.path()), vec!["lang-typescript"]);
    }

    #[test]
    fn detects_python_from_pyproject() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(detect_standard_names_in(dir.path()), vec!["lang-python"]);
    }

    #[test]
    fn detects_multiple_stacks_in_a_polyglot_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        let names = detect_standard_names_in(dir.path());
        assert!(names.contains(&"lang-rust".to_string()));
        assert!(names.contains(&"lang-python".to_string()));
    }

    #[test]
    fn empty_dir_detects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_standard_names_in(dir.path()).is_empty());
    }
}
