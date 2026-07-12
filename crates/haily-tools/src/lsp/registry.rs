//! Language → language-server mapping, PATH discovery, and per-language enable/disable.
//!
//! The registry is the single source of truth for "which server binary drives which language,
//! and is it installed on this host?". It is deliberately data-only (no process spawning) so it
//! is fully unit-testable without any server present — the default state of CI and this host
//! (see [`crate::lsp`] graceful-degradation contract).

use crate::coding::stack_detect::Stack;
use std::path::{Path, PathBuf};

/// One language server: the language key it serves and the argv used to launch it. The `program`
/// is a bare command name resolved against `PATH` ([`discover_on_path`]); a language whose program
/// is absent from `PATH` is treated as DISABLED (the tools no-op with a clear message rather than
/// hard-failing — the whole point of the graceful-degradation default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspServerSpec {
    /// Stable lowercase language key (matches [`Stack::lsp_language`]).
    pub language: &'static str,
    /// Server executable name (resolved on `PATH`).
    pub program: String,
    /// Launch arguments (most servers need `--stdio` or similar; rust-analyzer/gopls take none).
    pub args: Vec<String>,
}

impl LspServerSpec {
    fn new(language: &'static str, program: &str, args: &[&str]) -> Self {
        Self {
            language,
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// The built-in default server for each supported language. Extend this table as more servers are
/// curated — it mirrors the shape of `stack_detect`'s marker table. Kept minimal (YAGNI): the
/// languages the coding pipeline actually detects today, plus Go/Java which `stack_detect` now
/// detects. A caller may override the program via [`server_for_language`]'s env hook.
fn default_spec(language: &str) -> Option<LspServerSpec> {
    Some(match language {
        "rust" => LspServerSpec::new("rust", "rust-analyzer", &[]),
        // pyright ships the LSP entrypoint as `pyright-langserver`; `pylsp` is the fallback most
        // distros package, but we pick pyright first (richer type diagnostics for weak-model help).
        "python" => LspServerSpec::new("python", "pyright-langserver", &["--stdio"]),
        "typescript" => {
            LspServerSpec::new("typescript", "typescript-language-server", &["--stdio"])
        }
        "go" => LspServerSpec::new("go", "gopls", &[]),
        "java" => LspServerSpec::new("java", "jdtls", &[]),
        _ => return None,
    })
}

/// Env var naming a per-language program override, e.g. `HAILY_LSP_RUST=/opt/ra/rust-analyzer`.
/// Lets a host point Haily at a non-`PATH` server or swap pyright→pylsp without a code change.
fn override_env_key(language: &str) -> String {
    format!("HAILY_LSP_{}", language.to_uppercase())
}

/// Resolve the server spec for `language`, applying an optional `HAILY_LSP_<LANG>` program
/// override. Returns `None` for a language with no known server AND no override — the caller must
/// treat `None` as "unsupported language → degrade", never as an error.
pub fn server_for_language(language: &str) -> Option<LspServerSpec> {
    let base = default_spec(language);
    match std::env::var(override_env_key(language)) {
        Ok(prog) if !prog.trim().is_empty() => {
            // Keep the default args (a `--stdio`-style flag) when only the program is overridden;
            // if the language has no default, the override still yields a bare, arg-less spec.
            let language_key = base.as_ref().map(|s| s.language).unwrap_or_else(|| leak(language));
            let args = base.map(|s| s.args).unwrap_or_default();
            Some(LspServerSpec { language: language_key, program: prog, args })
        }
        _ => base,
    }
}

/// Convenience: the server spec for a detected [`Stack`].
pub fn server_for_stack(stack: Stack) -> Option<LspServerSpec> {
    server_for_language(stack.lsp_language())
}

/// Whether the spec's `program` is discoverable on `PATH` (or is an absolute path that exists).
/// A `false` here is the DISABLED signal that drives graceful degradation — never a failure.
pub fn is_available(spec: &LspServerSpec) -> bool {
    discover_on_path(&spec.program).is_some()
}

/// Resolve a bare command name against the `PATH` env var (plus Windows `PATHEXT` extensions),
/// or accept an absolute/relative path that already exists. Returns the resolved absolute path,
/// or `None` when the program is not found — the fail-soft signal, never an error.
pub fn discover_on_path(program: &str) -> Option<PathBuf> {
    let p = Path::new(program);
    // An explicit path (from an override) is used verbatim if it exists.
    if p.is_absolute() || program.contains('/') || program.contains('\\') {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    let exts = executable_extensions();
    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let cand = dir.join(format!("{program}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Candidate executable suffixes: `""` everywhere, plus the Windows `PATHEXT` set so a bare
/// `rust-analyzer` resolves `rust-analyzer.exe`/`.cmd`/`.bat`.
fn executable_extensions() -> Vec<String> {
    // `mut` is only exercised by the cfg(windows) block below.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut exts = vec![String::new()];
    #[cfg(windows)]
    {
        if let Ok(pathext) = std::env::var("PATHEXT") {
            for e in pathext.split(';').filter(|s| !s.is_empty()) {
                // PATHEXT entries include the leading dot (".EXE") — lower-cased for join.
                exts.push(e.to_ascii_lowercase());
            }
        } else {
            for e in [".exe", ".cmd", ".bat"] {
                exts.push(e.to_string());
            }
        }
    }
    exts
}

/// Leak a `&str` to `&'static str` — only used for the rare override-of-an-unknown-language path,
/// which is bounded by the finite set of languages a workspace can detect.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_each_supported_language_to_a_server() {
        // The mapping is the core registry contract: every language the pipeline detects resolves
        // to a concrete server command.
        for (lang, prog) in [
            ("rust", "rust-analyzer"),
            ("python", "pyright-langserver"),
            ("typescript", "typescript-language-server"),
            ("go", "gopls"),
            ("java", "jdtls"),
        ] {
            let spec = server_for_language(lang).unwrap_or_else(|| panic!("no server for {lang}"));
            assert_eq!(spec.program, prog);
            assert_eq!(spec.language, lang);
        }
    }

    #[test]
    fn maps_detected_stack_to_a_server() {
        assert_eq!(server_for_stack(Stack::Rust).unwrap().program, "rust-analyzer");
        assert_eq!(server_for_stack(Stack::Go).unwrap().program, "gopls");
    }

    #[test]
    fn unknown_language_has_no_server_and_degrades() {
        // A language with no known server must return None (→ caller degrades), never a bogus spec.
        assert!(server_for_language("brainfuck").is_none());
    }

    #[test]
    fn absent_program_reports_unavailable_not_error() {
        // The degradation signal: a server whose binary is not on PATH is DISABLED, never a panic.
        let spec = LspServerSpec::new("rust", "definitely-not-a-real-server-xyz", &[]);
        assert!(!is_available(&spec), "an absent server binary must report unavailable");
    }

    #[test]
    fn discovers_a_program_that_exists_on_path() {
        // `cargo`/`git` are guaranteed present in this repo's CI. Use one to prove PATH discovery
        // resolves a real binary (the positive half of the degradation gate).
        let found = discover_on_path("cargo").or_else(|| discover_on_path("git"));
        assert!(found.is_some(), "PATH discovery must resolve a known-present binary");
    }

    #[test]
    fn env_override_replaces_the_program() {
        // A host can point Haily at a non-PATH server. Use a temp file as the fake binary so the
        // absolute-path branch of discover_on_path accepts it.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("my-rust-analyzer");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        std::env::set_var("HAILY_LSP_RUST", &fake);
        let spec = server_for_language("rust").unwrap();
        std::env::remove_var("HAILY_LSP_RUST");
        assert_eq!(Path::new(&spec.program), fake);
        assert!(is_available(&spec), "an overridden absolute path that exists is available");
    }
}
