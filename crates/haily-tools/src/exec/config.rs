//! Sandbox configuration, exec request/output shapes, and the env allowlist.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Cap on captured child output per stream (goclaw `MaxOutputBytes`). A runaway build log
/// must never OOM the host; output past this is truncated and flagged.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024; // 1 MiB per stream

/// Default wall-clock ceiling for a single sandboxed exec when the request omits one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Environment variables passed through to the child, by name — an ALLOWLIST, never a
/// denylist (red-team: a denylist misses `SSH_AUTH_SOCK`, `GH_TOKEN`, `AWS_*`,
/// `CARGO_REGISTRY_TOKEN`, cloud LLM keys held by the parent process). `HOME`/`TMP`/`TEMP`
/// are NOT here because they are overridden to the scratch dir by [`build_child_env`].
const ALLOWED_ENV_KEYS: &[&str] = &["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TERM"];

/// Credential-bearing vars that MUST NOT leak into the child. Not consulted at runtime (the
/// allowlist already excludes everything not named above) — this is the canary set the
/// allowlist is tested against, so a future well-meaning add of a broad key is caught.
#[cfg(test)]
const DENIED_ENV_CANARIES: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "SSH_AUTH_SOCK",
    "CARGO_REGISTRY_TOKEN",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
];

/// Whether the child may reach the network. A **per-invocation profile field, not a global**
/// (red-team requirement): build/code-exec run [`NetworkPolicy::Off`] (exfiltration is the
/// top threat); the browser tool (P13) runs [`NetworkPolicy::On`] with a distinct profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkPolicy {
    /// No network access. The default for all code-exec and build.
    #[default]
    Off,
    /// Full network access — reserved for the browser tool (P13), which MUST reach the web.
    On,
}

/// Filesystem permission granted to the work root inside the sandbox (goclaw granularity).
///
/// RESERVED — declared for the config surface but not yet enforced by any exec path (WSL/Null
/// ignore it; native is stubbed). A caller must NOT assume `ReadOnly` confines writes until P1
/// wires it (native: read-only mount; WSL: ro bind). Until then, write confinement rests solely
/// on the backend's work-root isolation, not on this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxAccess {
    /// No filesystem access to the work root (pure compute).
    None,
    /// Read-only.
    ReadOnly,
    /// Read/write — the default for a build that produces artifacts.
    #[default]
    ReadWrite,
}

/// Which agents/domains get sandboxed (goclaw `Mode`), decoupled from the runtime backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Sandboxing disabled (only meaningful in explicitly-trusted dev contexts).
    Off,
    /// Sandbox every non-main (delegated/sub-agent) execution.
    NonMain,
    /// Sandbox all execution, including the root orchestrator's.
    #[default]
    All,
}

/// Pool lifetime of a reused sandbox (goclaw scope). A WSL2 distro is expensive to boot, so
/// the [`super::Manager`] reuses one across turns within the same scope rather than per-call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// One sandbox per session, reused across every turn in that session.
    Session,
    /// One sandbox per (sub-)agent.
    Agent,
    /// A single process-wide shared sandbox.
    Shared,
}

/// The [`super::Manager`] pool key: a scope plus its instance id (session/agent uuid, or a
/// fixed marker for `Shared`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScopeKey {
    scope_tag: &'static str,
    id: String,
}

impl ScopeKey {
    pub fn session(id: impl Into<String>) -> Self {
        Self { scope_tag: "session", id: id.into() }
    }
    pub fn agent(id: impl Into<String>) -> Self {
        Self { scope_tag: "agent", id: id.into() }
    }
    pub fn shared() -> Self {
        Self { scope_tag: "shared", id: String::new() }
    }
    pub fn from_scope(scope: Scope, id: impl Into<String>) -> Self {
        match scope {
            Scope::Session => Self::session(id),
            Scope::Agent => Self::agent(id),
            Scope::Shared => Self::shared(),
        }
    }
}

/// Static configuration of a sandbox instance (decoupled from a single [`ExecRequest`]).
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    pub access: SandboxAccess,
    pub network: NetworkPolicy,
    /// Per-invocation default timeout when a request omits one.
    pub timeout: Duration,
    /// Optional PID-count cap inside the sandbox (fork-bomb defense). RESERVED — not yet
    /// enforced by any exec path; P1 wires it (WSL: cgroup/`ulimit -u`; native: rlimit). Do not
    /// assume it confines fork behavior until then.
    pub pid_limit: Option<u32>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::default(),
            access: SandboxAccess::default(),
            network: NetworkPolicy::default(),
            timeout: DEFAULT_TIMEOUT,
            pid_limit: Some(512),
        }
    }
}

/// One command to run under a sandbox. `program` + `args` are passed as argv (never a
/// shell string) so there is no shell-interpolation surface.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    pub program: String,
    pub args: Vec<String>,
    /// The confinement root. The child sees this as its writable working dir.
    pub work_root: PathBuf,
    /// Extra env pairs (e.g. `WithEnv` credential injection) layered ON TOP of the
    /// allowlisted base — passed as env pairs, never embedded in the command string.
    pub env_overrides: Vec<(String, String)>,
    /// Overrides [`SandboxConfig::timeout`] for this call.
    pub timeout: Option<Duration>,
}

impl ExecRequest {
    pub fn new(program: impl Into<String>, work_root: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            work_root: work_root.into(),
            env_overrides: Vec::new(),
            timeout: None,
        }
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }
}

/// Result of a completed sandboxed exec. A non-zero `status` is a normal outcome, not an error.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    /// Child exit code (or a synthetic negative code if killed by signal / unavailable).
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    /// True if either stream was capped at [`MAX_OUTPUT_BYTES`].
    pub truncated: bool,
    pub backend: super::SandboxKind,
}

/// Build the child's environment as an allowlist: the named [`ALLOWED_ENV_KEYS`] copied from
/// the parent (if present), `HOME`/`TMP`/`TEMP`/`TMPDIR` forced to `scratch_home`, then the
/// caller's `extra` overrides applied last. Nothing else from the parent env reaches the child.
pub fn build_child_env(scratch_home: &Path, extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    for key in ALLOWED_ENV_KEYS {
        if let Ok(val) = std::env::var(key) {
            env.push(((*key).to_string(), val));
        }
    }
    let scratch = scratch_home.to_string_lossy().to_string();
    for key in ["HOME", "TMP", "TEMP", "TMPDIR"] {
        env.push((key.to_string(), scratch.clone()));
    }
    for (k, v) in extra {
        env.push((k.clone(), v.clone()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_env_excludes_all_credential_canaries() {
        // Simulate a parent process holding secrets.
        for c in DENIED_ENV_CANARIES {
            std::env::set_var(c, "SECRET");
        }
        let env = build_child_env(Path::new("/scratch"), &[]);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        for c in DENIED_ENV_CANARIES {
            assert!(!keys.contains(c), "credential var {c:?} leaked into child env");
        }
        for c in DENIED_ENV_CANARIES {
            std::env::remove_var(c);
        }
    }

    #[test]
    fn child_env_forces_scratch_home() {
        let env = build_child_env(Path::new("/scratch"), &[]);
        let home = env.iter().find(|(k, _)| k == "HOME").map(|(_, v)| v.clone());
        assert_eq!(home.as_deref(), Some("/scratch"));
    }

    #[test]
    fn child_env_applies_overrides_last() {
        let env = build_child_env(Path::new("/scratch"), &[("MYVAR".into(), "1".into())]);
        assert!(env.iter().any(|(k, v)| k == "MYVAR" && v == "1"));
    }

    #[test]
    fn scope_key_distinguishes_scopes() {
        assert_ne!(ScopeKey::session("a"), ScopeKey::agent("a"));
        assert_eq!(ScopeKey::shared(), ScopeKey::from_scope(Scope::Shared, "ignored"));
    }
}
