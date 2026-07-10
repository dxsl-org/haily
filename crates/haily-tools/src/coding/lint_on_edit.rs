//! Per-edit syntax gate (SWE-agent's +10.7pp ACI mechanism): after every `fs_write`/`fs_edit`
//! the intent is to run a cheap tree-sitter parse of the file's language and reject the edit on
//! a syntax error at the cheapest point (before any compile gate), ADDITIVE to the P6 compile
//! gate.
//!
//! STATUS (Sub-Agent + Skill Architecture phase 1 — DEVIATION, logged): the tree-sitter
//! grammar crates could not be added to this workspace. Adding `tree-sitter*` forces cargo to
//! re-resolve the dependency graph and DOWNGRADE `sqlx` (0.8.6 → 0.8.0), which pulls a second
//! `libsqlite3-sys` (0.28 vs the pinned 0.30) — two crates linking `sqlite3`, an unresolvable
//! `links` collision. Rather than repin the whole DB stack (out of P1 scope, high blast
//! radius), lint-on-edit currently SKIPS (returns `Ok`) for every file.
//!
//! This is EXACTLY the plan's sanctioned degradation ("a grammar that is unavailable/fails to
//! build simply SKIPS lint (log) — never fail a legitimate edit for a missing grammar"): a
//! missing grammar must never reject a valid edit, so skip is the fail-safe. The compile gate
//! (`shell_exec cargo check`) still catches syntax errors, one step later. The API and the
//! extension→language map below are preserved so re-enabling the parse is a localized change
//! once the sqlx/libsqlite3-sys constraint is resolved.

/// Languages the parse gate would cover once a grammar is wired. Kept as the single source of
/// truth for "is this a language we lint" so re-enabling only swaps the body of [`check`].
fn is_lintable_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "rs" | "py" | "pyi" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "mts" | "cts" | "tsx" | "go"
    )
}

/// Reject `content` if its language grammar reports a syntax error; otherwise `Ok`.
///
/// Currently ALWAYS `Ok` (grammar unavailable — see module docs): a missing grammar must never
/// fail a legitimate edit. When tree-sitter is wired, only a loadable grammar that finds a
/// parse error will return `Err`; an unknown extension or unloadable grammar stays `Ok`.
///
/// # Errors
/// Returns an error only when (future) grammar parsing finds a syntax error.
pub fn check(rel_path: &str, _content: &str) -> anyhow::Result<()> {
    let ext = std::path::Path::new(rel_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if is_lintable_ext(ext) {
        // Grammar not compiled in — skip (fail-safe), but leave a breadcrumb at trace level so
        // the gap is observable rather than silent.
        tracing::trace!(ext, "lint-on-edit: grammar unavailable, skipping parse gate");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_fails_a_legit_edit_even_for_lintable_language() {
        // The core invariant under the stub: a missing grammar must NEVER reject an edit.
        assert!(check("src/main.rs", "fn main() { let x = ").is_ok());
        assert!(check("app.py", "def f(:\n  return").is_ok());
    }

    #[test]
    fn unknown_extension_skips() {
        assert!(check("notes.md", "whatever {{{").is_ok());
    }

    #[test]
    fn lintable_extensions_recognized() {
        assert!(is_lintable_ext("rs"));
        assert!(is_lintable_ext("TS"));
        assert!(!is_lintable_ext("md"));
    }
}
