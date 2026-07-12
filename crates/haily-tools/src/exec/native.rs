//! Native process-isolation backends for macOS and Linux — the cross-platform seam.
//!
//! The gate phase ships the **structs, the trait impls, and the profile/argv builders**
//! (unit-tested) so the seam is real, but their `exec` returns [`SandboxError::BackendUnavailable`]:
//! full process wiring lands with each platform's CI. On those platforms the [`super::Manager`]
//! therefore selects [`super::NullSandbox`] (fail-safe first-exec approval) until this is
//! completed — never a silent unsandboxed run.

use super::config::{ExecOutput, ExecRequest, NetworkPolicy, SandboxConfig};
use super::sandbox::{Sandbox, SandboxKind};
use super::SandboxError;
use async_trait::async_trait;
use std::path::Path;

/// macOS Seatbelt (`sandbox-exec`) backend.
#[derive(Default)]
pub struct MacSandbox;

impl MacSandbox {
    pub fn new() -> Self {
        Self
    }
}

/// Build a Seatbelt (`.sb`) profile: deny by default, allow reads everywhere but writes only
/// under `work_root`, and deny network unless `network` is `On`.
///
/// HARDENING TODO (before macOS CI activates `exec`): `(allow file-read*)` is too broad — it
/// lets sandboxed code read `~/.aws/credentials`, SSH keys, etc. `(deny network*)` blocks direct
/// exfil, but a build can still copy a secret into a work-root artifact. Restrict reads to
/// `work_root` + the minimal system prefixes a toolchain needs before this profile goes live.
pub fn seatbelt_profile(work_root: &Path, network: NetworkPolicy) -> String {
    let root = work_root.display();
    let net = match network {
        NetworkPolicy::Off => "(deny network*)",
        NetworkPolicy::On => "(allow network*)",
    };
    format!(
        "(version 1)\n\
         (deny default)\n\
         (allow process-exec process-fork)\n\
         (allow file-read*)\n\
         (allow file-write* (subpath \"{root}\"))\n\
         {net}\n"
    )
}

#[async_trait]
impl Sandbox for MacSandbox {
    fn kind(&self) -> SandboxKind {
        SandboxKind::MacSeatbelt
    }
    fn is_enforcing(&self) -> bool {
        true
    }
    async fn exec(
        &self,
        _req: ExecRequest,
        _cfg: &SandboxConfig,
    ) -> Result<ExecOutput, SandboxError> {
        Err(SandboxError::BackendUnavailable(
            "MacSandbox exec lands with macOS CI; Manager falls back to NullSandbox".into(),
        ))
    }
}

/// Linux user-namespaces + seccomp (`bwrap` where present) backend.
#[derive(Default)]
pub struct LinuxNamespaceSandbox;

impl LinuxNamespaceSandbox {
    pub fn new() -> Self {
        Self
    }
}

/// Build a `bwrap` argv: read-only bind of `/usr` + `/bin`, a read/write bind of `work_root`,
/// a private `/tmp`, and network unshared unless `network` is `On`.
pub fn bwrap_argv(work_root: &Path, network: NetworkPolicy) -> Vec<String> {
    let root = work_root.display().to_string();
    let mut argv = vec![
        "--ro-bind".into(), "/usr".into(), "/usr".into(),
        "--ro-bind".into(), "/bin".into(), "/bin".into(),
        "--ro-bind".into(), "/lib".into(), "/lib".into(),
        "--bind".into(), root.clone(), root.clone(),
        "--tmpfs".into(), "/tmp".into(),
        "--proc".into(), "/proc".into(),
        "--die-with-parent".into(),
        "--new-session".into(),
    ];
    if network == NetworkPolicy::Off {
        argv.push("--unshare-net".into());
    }
    argv
}

#[async_trait]
impl Sandbox for LinuxNamespaceSandbox {
    fn kind(&self) -> SandboxKind {
        SandboxKind::LinuxNamespace
    }
    fn is_enforcing(&self) -> bool {
        true
    }
    async fn exec(
        &self,
        _req: ExecRequest,
        _cfg: &SandboxConfig,
    ) -> Result<ExecOutput, SandboxError> {
        Err(SandboxError::BackendUnavailable(
            "LinuxNamespaceSandbox exec lands with Linux CI; Manager falls back to NullSandbox".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seatbelt_denies_network_by_default_and_confines_writes() {
        let p = seatbelt_profile(Path::new("/work"), NetworkPolicy::Off);
        assert!(p.contains("(deny default)"));
        assert!(p.contains("(deny network*)"));
        assert!(p.contains("(subpath \"/work\")"));

        let on = seatbelt_profile(Path::new("/work"), NetworkPolicy::On);
        assert!(on.contains("(allow network*)"));
    }

    #[test]
    fn bwrap_unshares_net_when_off_only() {
        let off = bwrap_argv(Path::new("/work"), NetworkPolicy::Off);
        assert!(off.contains(&"--unshare-net".to_string()));
        assert!(off.contains(&"/work".to_string()));

        let on = bwrap_argv(Path::new("/work"), NetworkPolicy::On);
        assert!(!on.contains(&"--unshare-net".to_string()));
    }

    #[tokio::test]
    async fn stub_exec_is_honestly_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let r = MacSandbox::new()
            .exec(ExecRequest::new("true", dir.path()), &SandboxConfig::default())
            .await;
        assert!(matches!(r, Err(SandboxError::BackendUnavailable(_))));
    }
}
