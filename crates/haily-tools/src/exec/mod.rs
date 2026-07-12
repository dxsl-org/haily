//! General-purpose execution sandbox seam (Phase 0 gate).
//!
//! This module is **domain-agnostic on purpose**. The moment ANY domain is granted
//! code execution to solve a problem — the coding pipeline running `cargo`/`npm`/`pytest`,
//! a researcher running a data script, finance running a calculation — it executes through
//! the SAME `Sandbox` seam here, never a bespoke `std::process::Command` call. This is
//! harness-first applied to execution: every present or future code-exec surface inherits
//! isolation, an env allowlist, output caps, and fail-safe approval for free.
//!
//! # Why a sandbox at all (the load-bearing security premise)
//! The plan's original assumption that "the verifier is a safe `ReversibleWrite`" was false:
//! `cargo check`/`test`, `npm`, and `pytest` execute *attacker-authored* code at run time
//! (`build.rs`, proc-macros, npm lifecycle scripts, `conftest.py`). `RiskTier`/path-guards
//! govern tool CALLS, never the code a tool executes. The sandbox is therefore the single
//! primary containment for arbitrary code execution; everything else is defense-in-depth.
//!
//! # Backends (all behind the one [`Sandbox`] trait)
//! - [`wsl2::Wsl2Sandbox`] — Windows default: a dedicated managed WSL2 distro, networking-off,
//!   scratch mount + scratch HOME.
//! - [`native::MacSandbox`] / [`native::LinuxNamespaceSandbox`] — real cross-platform seam
//!   (argv/profile builders implemented + tested); full `exec` lands with those platforms' CI.
//! - [`null::NullSandbox`] — explicit fail-safe: refuses to auto-approve, forces first-exec
//!   approval per work root. Selected when no enforcing backend is available. NEVER silent
//!   unsandboxed exec.
//!
//! A `WasmSandbox` (wasmtime, pure-compute `code_exec`) is a designed-for future backend —
//! the trait's per-invocation network profile and the [`SandboxKind::Wasm`] variant reserve
//! its slot — but it is deliberately NOT compiled in this gate phase (adding wasmtime before
//! `code_exec` ships in P1 is premature weight). See the Phase 0 spike report.

pub mod code_exec;
pub mod config;
pub mod manager;
pub mod native;
pub mod null;
pub mod sandbox;
mod spawn;
pub mod wsl2;

pub use config::{
    build_child_env, ExecOutput, ExecRequest, NetworkPolicy, SandboxAccess, SandboxConfig,
    SandboxMode, Scope, ScopeKey, DEFAULT_TIMEOUT, MAX_OUTPUT_BYTES,
};
pub use manager::{Manager, ManagerStats};
pub use null::NullSandbox;
/// Cancellation-aware spawn (Sub-Agent + Skill Architecture phase 1) — used by the coding
/// `shell_exec`/`code_exec` non-enforcing path to honor the kill switch mid-run.
pub(crate) use spawn::spawn_capture_cancellable;
pub use sandbox::{
    build_browser_env, browser_sandbox_config, detect_redirection_triggers, RedirectionTrigger,
    Sandbox, SandboxKind, BROWSER_CDP_BIND_ADDR,
};

use std::path::PathBuf;

/// The ONLY Haily tools that code executing *inside* a sandbox may call back into (hermes
/// `SANDBOX_ALLOWED_TOOLS` pattern — a positive allowlist, never a denylist). It is
/// deliberately a small set of pure reads: a snippet that needs to look something up can,
/// but it can never reach a destructive or egress surface from inside the sandbox.
///
/// Invariant (enforced by test `denied_tools_never_in_allowlist`): no delete, undo,
/// forget, or apply tool ever appears here. Adding one is a security regression.
pub const SANDBOX_ALLOWED_TOOLS: &[&str] = &[
    "memory_search",
    "memory_list",
    "note_search",
    "task_list",
    "reminder_list",
    "calendar_list",
    "work_item_list",
];

/// Tools that must NEVER be reachable from inside the sandbox, asserted against
/// [`SANDBOX_ALLOWED_TOOLS`] in tests. Not exhaustive of all destructive tools — it is the
/// canary set the allowlist is checked against.
#[cfg(test)]
const SANDBOX_DENIED_CANARIES: &[&str] = &[
    "memory_forget",
    "task_delete",
    "note_delete",
    "reminder_delete",
    "calendar_delete",
    "work_item_delete",
    "worktree_apply",
    "journal_undo",
];

/// True iff `tool_name` may be called back from inside the sandbox.
pub fn is_callback_allowed(tool_name: &str) -> bool {
    SANDBOX_ALLOWED_TOOLS.contains(&tool_name)
}

/// Sandbox-level failures. A non-zero *child* exit is NOT an error — it is a valid
/// [`ExecOutput`]; these variants are failures of the sandbox itself.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The only-safe outcome under [`NullSandbox`] (and any non-enforcing backend) when a
    /// work root has not yet been approved for execution. The caller MUST route this through
    /// the `ApprovalGate` and, only if approved, retry — never silently downgrade to a raw spawn.
    #[error("execution in {work_root:?} requires first-exec approval (no enforcing sandbox)")]
    ApprovalRequired { work_root: PathBuf },

    /// The selected backend is present as a seam but not executable on this host/build
    /// (e.g. the native macOS/Linux backends before their platform CI lands).
    #[error("sandbox backend unavailable: {0}")]
    BackendUnavailable(String),

    /// Wall-clock timeout tripped before the child exited.
    #[error("sandbox execution timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// Failed to spawn the child process at all.
    #[error("failed to spawn sandboxed process: {0}")]
    Spawn(#[source] std::io::Error),

    /// The requested work root is missing or is not a directory.
    #[error("invalid work root: {0:?}")]
    InvalidWorkRoot(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denied_tools_never_in_allowlist() {
        for denied in SANDBOX_DENIED_CANARIES {
            assert!(
                !is_callback_allowed(denied),
                "SECURITY REGRESSION: destructive/egress tool {denied:?} is callable from inside the sandbox"
            );
        }
    }

    #[test]
    fn allowlist_members_are_recognized() {
        assert!(is_callback_allowed("note_search"));
        assert!(!is_callback_allowed("nonexistent_tool"));
    }
}
