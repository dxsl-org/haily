//! Classification of a `shell_exec` command into a risk tier.
//!
//! VERIFIER commands (build/test/lint/format that produce no lasting side effect outside the
//! workspace) are `ReversibleWrite` and may auto-run — but ONLY when the selected sandbox is
//! enforcing (red-team SEC-C1: `cargo test` et al. execute attacker-authored `build.rs`/
//! proc-macros/`conftest.py`/npm scripts; auto-approving that UNSANDBOXED is RCE). Everything
//! else is `IrreversibleWrite` (approval-gated). The classification keys on `(program, first
//! subcommand)` — a positive allowlist, never a denylist.

/// A recognized verifier: its program plus the set of subcommands that count as verification.
struct Verifier {
    program: &'static str,
    /// Allowed first subcommands. Empty = program itself with no subcommand gate (e.g.
    /// `pytest`, whose "subcommand" is really just test-selection args).
    subcommands: &'static [&'static str],
}

/// The closed set of verifier commands. Deliberately conservative: build/test/lint/format
/// for the languages Haily targets. Anything not matched here is `IrreversibleWrite`.
const VERIFIERS: &[Verifier] = &[
    Verifier { program: "cargo", subcommands: &["check", "clippy", "test", "fmt", "build"] },
    Verifier { program: "npm", subcommands: &["test", "run"] },
    Verifier { program: "pnpm", subcommands: &["test", "run"] },
    Verifier { program: "yarn", subcommands: &["test", "run"] },
    Verifier { program: "pytest", subcommands: &[] },
    Verifier { program: "python", subcommands: &["-m"] }, // `python -m pytest` etc.
    Verifier { program: "go", subcommands: &["test", "build", "vet"] },
    Verifier { program: "tsc", subcommands: &[] },
];

/// True if `(program, args)` is a recognized verifier command.
///
/// For `npm/pnpm/yarn run`, only `run build`/`run test`/`run lint`/`run typecheck` are
/// verifiers — an arbitrary `run <script>` could do anything, so it stays IrreversibleWrite.
pub fn is_verifier(program: &str, args: &[String]) -> bool {
    let base = program_basename(program);
    for v in VERIFIERS {
        if v.program != base {
            continue;
        }
        if v.subcommands.is_empty() {
            return true;
        }
        let Some(first) = args.first().map(|s| s.as_str()) else {
            return false;
        };
        if !v.subcommands.contains(&first) {
            continue;
        }
        // Narrow the npm/pnpm/yarn `run <script>` case to known verification scripts.
        if first == "run" {
            return matches!(
                args.get(1).map(|s| s.as_str()),
                Some("build") | Some("test") | Some("lint") | Some("typecheck") | Some("check")
            );
        }
        return true;
    }
    false
}

/// Strip a directory + `.exe` suffix so `C:\...\cargo.exe` and `/usr/bin/cargo` both match
/// `cargo`.
fn program_basename(program: &str) -> &str {
    let after_sep = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program);
    after_sep.strip_suffix(".exe").unwrap_or(after_sep)
}

/// True if the program is `cargo` (basename) — the caller prepends `cargo_safe_args()`.
pub fn is_cargo(program: &str) -> bool {
    program_basename(program) == "cargo"
}

/// True if the program is `git` (basename) — the caller prepends `git_safe_args()`.
pub fn is_git(program: &str) -> bool {
    program_basename(program) == "git"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recognizes_core_verifiers() {
        assert!(is_verifier("cargo", &a(&["check"])));
        assert!(is_verifier("cargo", &a(&["clippy", "--", "-D", "warnings"])));
        assert!(is_verifier("cargo", &a(&["test"])));
        assert!(is_verifier("pytest", &a(&["-q"])));
        assert!(is_verifier("go", &a(&["test", "./..."])));
        assert!(is_verifier("tsc", &a(&["--noEmit"])));
    }

    #[test]
    fn npm_run_only_verifier_scripts() {
        assert!(is_verifier("npm", &a(&["test"])));
        assert!(is_verifier("npm", &a(&["run", "build"])));
        assert!(is_verifier("npm", &a(&["run", "test"])));
        // An arbitrary run-script is NOT a verifier (could do anything).
        assert!(!is_verifier("npm", &a(&["run", "deploy"])));
        assert!(!is_verifier("npm", &a(&["install"])));
    }

    #[test]
    fn non_verifiers_rejected() {
        assert!(!is_verifier("rm", &a(&["-rf", "/"])));
        assert!(!is_verifier("curl", &a(&["http://x"])));
        assert!(!is_verifier("cargo", &a(&["publish"])));
        assert!(!is_verifier("cargo", &a(&[]))); // bare cargo is not a verifier
    }

    #[test]
    fn basename_normalizes_path_and_exe() {
        assert!(is_verifier("/usr/bin/cargo", &a(&["build"])));
        assert!(is_verifier("C:\\rust\\cargo.exe", &a(&["build"])));
        assert!(is_cargo("cargo.exe"));
        assert!(is_git("/usr/bin/git"));
    }
}
