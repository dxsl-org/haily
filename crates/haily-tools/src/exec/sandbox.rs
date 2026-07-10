//! The `Sandbox` trait, backend identity, and config-redirection defense.

use super::config::{ExecOutput, ExecRequest, SandboxConfig};
use super::SandboxError;
use async_trait::async_trait;
use std::path::Path;

/// Which concrete backend produced an [`ExecOutput`] — recorded so the spike telemetry and
/// the per-attempt egress log can attribute a run to its actual isolation level (a "local"
/// baseline that silently fell to `Null` must be visible, not assumed enforcing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxKind {
    Wsl2,
    MacSeatbelt,
    LinuxNamespace,
    /// Reserved for the future wasmtime pure-compute backend (not compiled in the gate phase).
    Wasm,
    /// The fail-safe non-enforcing backend.
    Null,
}

/// Uniform interface over every isolation backend. Both `shell_exec` (coding) and the
/// domain-agnostic `code_exec` (any domain) go through this — there is no other legitimate
/// path to running untrusted code.
#[async_trait]
pub trait Sandbox: Send + Sync {
    fn kind(&self) -> SandboxKind;

    /// True iff this backend actually isolates. `false` for [`SandboxKind::Null`], which is
    /// the signal that execution requires explicit per-work-root approval instead of auto-run.
    fn is_enforcing(&self) -> bool;

    /// Run one command under this sandbox. A non-zero child exit returns `Ok(ExecOutput)`
    /// with that status; only sandbox-level failures return `Err`. A non-enforcing backend
    /// returns [`SandboxError::ApprovalRequired`] until the work root has been approved.
    async fn exec(
        &self,
        req: ExecRequest,
        cfg: &SandboxConfig,
    ) -> Result<ExecOutput, SandboxError>;
}

/// A run-time config-redirection vector the model could plant to escape the sandbox by
/// hijacking a tool's own execution (e.g. `.cargo/config.toml`'s `runner=`, a git hook, a
/// `build.rs`). Presence is an **approval trigger**, never an auto-approve — the sandbox
/// itself still contains the code, but surfacing these lets the user see the intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectionTrigger {
    /// An in-work-root `.cargo/config.toml` (can set `runner`/`rustc-wrapper`).
    CargoConfig,
    /// A non-sample file under `.git/hooks/` (fires on git operations).
    GitHook(String),
    /// A `build.rs` — arbitrary code run at build time by `cargo`.
    BuildScript,
    /// A crate declaring `proc-macro = true` — arbitrary code run at compile time.
    ProcMacro,
}

/// Cargo args that pin a trusted runner and scrub wrapper hijacks. The CALLER (P1's cargo-running
/// tool) MUST prepend these to its cargo invocation — nothing in the sandbox applies them
/// automatically — so an in-tree `.cargo/config.toml runner=` cannot redirect execution.
pub fn cargo_safe_args() -> Vec<String> {
    vec![
        "--config".into(),
        "target.'cfg(all())'.runner=''".into(),
        "--config".into(),
        "build.rustc-wrapper=''".into(),
    ]
}

/// Git args that neutralize an in-tree hooks path (`core.hooksPath` → empty dir semantics via
/// an explicit override to a path with no hooks).
pub fn git_safe_args(empty_hooks_dir: &Path) -> Vec<String> {
    vec![
        "-c".into(),
        format!("core.hooksPath={}", empty_hooks_dir.display()),
    ]
}

/// Scan `work_root` (TOP-LEVEL ONLY) for redirection vectors. Best-effort: an unreadable dir
/// yields no triggers rather than an error (the sandbox is the real control; this is advisory
/// surfacing). LIMITATION: in a Cargo/workspace layout, `build.rs`/proc-macro crates/
/// `.cargo/config.toml` often live in member subdirs — a shallow member walk is a P1 improvement;
/// do NOT treat an empty result as proof of no redirection vectors.
pub fn detect_redirection_triggers(work_root: &Path) -> Vec<RedirectionTrigger> {
    let mut out = Vec::new();

    if work_root.join(".cargo").join("config.toml").is_file()
        || work_root.join(".cargo").join("config").is_file()
    {
        out.push(RedirectionTrigger::CargoConfig);
    }

    let hooks = work_root.join(".git").join("hooks");
    if let Ok(entries) = std::fs::read_dir(&hooks) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Fresh clones ship `.sample` hooks that never fire — ignore those.
            if !name.ends_with(".sample") && entry.path().is_file() {
                out.push(RedirectionTrigger::GitHook(name));
            }
        }
    }

    if work_root.join("build.rs").is_file() {
        out.push(RedirectionTrigger::BuildScript);
    }

    if let Ok(manifest) = std::fs::read_to_string(work_root.join("Cargo.toml")) {
        // Cheap textual check: a proper TOML parse is overkill for an advisory trigger.
        if manifest
            .lines()
            .any(|l| l.replace(' ', "").starts_with("proc-macro=true"))
        {
            out.push(RedirectionTrigger::ProcMacro);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_args_pin_runner_and_wrapper() {
        let a = cargo_safe_args().join(" ");
        assert!(a.contains("runner=''"));
        assert!(a.contains("rustc-wrapper=''"));
    }

    #[test]
    fn detects_planted_cargo_config_and_build_script() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
        std::fs::write(dir.path().join(".cargo").join("config.toml"), "[target]\n").unwrap();
        std::fs::write(dir.path().join("build.rs"), "fn main() {}").unwrap();

        let t = detect_redirection_triggers(dir.path());
        assert!(t.contains(&RedirectionTrigger::CargoConfig));
        assert!(t.contains(&RedirectionTrigger::BuildScript));
    }

    #[test]
    fn ignores_sample_git_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-commit.sample"), "#!/bin/sh\n").unwrap();
        assert!(detect_redirection_triggers(dir.path()).is_empty());

        std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\necho hi").unwrap();
        let t = detect_redirection_triggers(dir.path());
        assert!(t.iter().any(|x| matches!(x, RedirectionTrigger::GitHook(n) if n == "pre-commit")));
    }

    #[test]
    fn detects_proc_macro_crate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[lib]\nproc-macro = true\n",
        )
        .unwrap();
        assert!(detect_redirection_triggers(dir.path()).contains(&RedirectionTrigger::ProcMacro));
    }
}
