//! `NullSandbox` — the explicit, fail-safe non-enforcing backend.
//!
//! Selected when no enforcing backend is available on the host (WSL2 absent on Windows;
//! locked-down namespaces on Linux; etc). Its whole reason to exist is the security
//! invariant **"never silently run unsandboxed"**: it refuses to execute a work root that
//! has not been explicitly approved, returning [`SandboxError::ApprovalRequired`] so the
//! caller must route through the `ApprovalGate`. Only after approval does it run the child —
//! and even then it still applies the env allowlist as defense-in-depth, honestly reporting
//! [`SandboxKind::Null`] so telemetry never mistakes it for real isolation.

use super::config::{build_child_env, ExecOutput, ExecRequest, SandboxConfig};
use super::sandbox::{Sandbox, SandboxKind};
use super::spawn::spawn_capture;
use super::SandboxError;
use async_trait::async_trait;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Default)]
pub struct NullSandbox {
    /// Work roots the user has approved for first-exec. Once present, exec runs (unsandboxed,
    /// honestly labeled). Guarded by a plain `Mutex` — approval traffic is low-frequency.
    approved_roots: Mutex<HashSet<PathBuf>>,
}

impl NullSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the user approved first-exec for `work_root`. Idempotent.
    pub fn approve(&self, work_root: PathBuf) {
        if let Ok(mut set) = self.approved_roots.lock() {
            set.insert(work_root);
        }
    }

    fn is_approved(&self, work_root: &PathBuf) -> bool {
        self.approved_roots
            .lock()
            .map(|s| s.contains(work_root))
            .unwrap_or(false)
    }
}

#[async_trait]
impl Sandbox for NullSandbox {
    fn kind(&self) -> SandboxKind {
        SandboxKind::Null
    }

    fn is_enforcing(&self) -> bool {
        false
    }

    async fn exec(
        &self,
        req: ExecRequest,
        cfg: &SandboxConfig,
    ) -> Result<ExecOutput, SandboxError> {
        if !self.is_approved(&req.work_root) {
            return Err(SandboxError::ApprovalRequired {
                work_root: req.work_root.clone(),
            });
        }
        // Approved: run best-effort with a scratch HOME = the work root and the env allowlist
        // still in force (no isolation, but no secret leak either).
        let env = build_child_env(&req.work_root, &req.env_overrides);
        let timeout = req.timeout.unwrap_or(cfg.timeout);
        spawn_capture(
            &req.program,
            &req.args,
            &req.work_root,
            &env,
            timeout,
            SandboxKind::Null,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unapproved_root_requires_approval_never_runs() {
        let dir = tempfile::tempdir().unwrap();
        let sb = NullSandbox::new();
        let req = ExecRequest::new("cmd", dir.path());
        let r = sb.exec(req, &SandboxConfig::default()).await;
        assert!(
            matches!(r, Err(SandboxError::ApprovalRequired { .. })),
            "NullSandbox must refuse to run an unapproved work root"
        );
    }

    #[tokio::test]
    async fn approved_root_runs() {
        let dir = tempfile::tempdir().unwrap();
        let sb = NullSandbox::new();
        sb.approve(dir.path().to_path_buf());

        #[cfg(windows)]
        let req = ExecRequest::new("cmd", dir.path()).args(["/C", "echo ok"]);
        #[cfg(not(windows))]
        let req = ExecRequest::new("sh", dir.path()).args(["-c", "echo ok"]);

        let out = sb.exec(req, &SandboxConfig::default()).await.expect("runs");
        assert_eq!(out.status, 0);
        assert_eq!(out.backend, SandboxKind::Null);
        assert!(out.stdout.contains("ok"));
    }

    #[test]
    fn is_not_enforcing() {
        assert!(!NullSandbox::new().is_enforcing());
    }
}
