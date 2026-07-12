//! The `Sandbox` trait, backend identity, and config-redirection defense.

use super::config::{
    build_child_env, ExecOutput, ExecRequest, NetworkPolicy, SandboxAccess, SandboxConfig,
    SandboxMode,
};
use super::SandboxError;
use async_trait::async_trait;
use std::path::Path;
use std::time::Duration;

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

/// The loopback address the P13 browser's CDP endpoint MUST bind to. The remote-debugging port
/// is a full remote-control channel over the browser; exposing it off-host would let anything on
/// the network drive the user's logged-in sessions. Callers pass this to
/// [`crate::browser::stealth::build_launch_flags`] as `--remote-debugging-address`.
pub const BROWSER_CDP_BIND_ADDR: &str = "127.0.0.1";

/// Build the **network-allowed** sandbox profile for the P13 stealth browser (the one exec
/// surface that MUST reach the web — DISTINCT from code-exec's [`NetworkPolicy::Off`] profile).
///
/// The profile is: network `On`; filesystem confined to the browser profile dir
/// ([`SandboxAccess::ReadWrite`] over that dir only — see [`build_browser_env`], which forces
/// `HOME`/`TMP` there so the browser cannot read the real home); credential env scrubbed via the
/// same allowlist as code-exec; and loopback-only CDP ([`BROWSER_CDP_BIND_ADDR`], pinned in the
/// launch flags). `timeout` bounds a single browser operation.
pub fn browser_sandbox_config(timeout: Duration) -> SandboxConfig {
    SandboxConfig {
        mode: SandboxMode::All,
        access: SandboxAccess::ReadWrite,
        network: NetworkPolicy::On,
        timeout,
        pid_limit: Some(512),
    }
}

/// Build the browser child's environment: the SAME credential-scrubbing allowlist as code-exec
/// ([`build_child_env`]), with `HOME`/`TMP`/`TEMP` forced to `profile_dir` so the browser writes
/// only inside its confined profile dir and no parent-process credential (`GH_TOKEN`, `AWS_*`,
/// cloud LLM keys, …) reaches it.
pub fn build_browser_env(profile_dir: &Path) -> Vec<(String, String)> {
    build_child_env(profile_dir, &[])
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

    // ---- P13 network-allowed browser profile -----------------------------------

    #[test]
    fn browser_profile_allows_network_unlike_code_exec() {
        let cfg = browser_sandbox_config(Duration::from_secs(60));
        // The browser MUST reach the web — the one profile with network On...
        assert_eq!(cfg.network, NetworkPolicy::On);
        // ...whereas the default (code-exec/build) profile is network Off.
        assert_eq!(SandboxConfig::default().network, NetworkPolicy::Off);
        assert_eq!(cfg.timeout, Duration::from_secs(60));
    }

    #[test]
    fn browser_env_scrubs_credentials_and_confines_home() {
        // Simulate a parent process holding secrets, then prove the browser child sees none of
        // them and has HOME/TMP forced into its confined profile dir. Canary set is local (the
        // config.rs one is module-private) but covers the same credential classes.
        const CANARIES: &[&str] = &[
            "GH_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
        ];
        for c in CANARIES {
            std::env::set_var(c, "SECRET");
        }
        let profile = Path::new("/scratch/haily-browser");
        let env = build_browser_env(profile);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        for c in CANARIES {
            assert!(!keys.contains(c), "credential var {c:?} leaked into the browser env");
        }
        let home = env.iter().find(|(k, _)| k == "HOME").map(|(_, v)| v.clone());
        assert_eq!(home.as_deref(), Some("/scratch/haily-browser"),
            "browser HOME must be confined to the profile dir");
        for c in CANARIES {
            std::env::remove_var(c);
        }
    }

    #[test]
    fn browser_cdp_binds_loopback_only() {
        assert_eq!(BROWSER_CDP_BIND_ADDR, "127.0.0.1");
    }
}
