//! `Wsl2Sandbox` — the Windows default backend.
//!
//! Runs the child inside a **dedicated managed WSL2 distro** (never the user's default distro),
//! provisioned networking-off with a scratch HOME. The Windows work root is bind-visible via
//! `/mnt/<drive>/…`; the child's env is rebuilt from scratch inside the distro with `env -i`.
//!
//! Distro selection is opt-in via `HAILY_WSL_DISTRO` (the name of the provisioned managed
//! distro). If unset, [`Wsl2Sandbox::detect`] returns `None` and the [`super::Manager`] falls
//! to [`super::NullSandbox`] — we deliberately do NOT hijack the user's default distro, whose
//! networking/state we do not control (a shared distro left network-on is a silent hole).
//!
//! HARD PROVISIONING REQUIREMENTS (this code assumes them; it cannot enforce them itself — P1
//! provisioning MUST establish them, else the confinement contract is UNMET):
//! - **Networking OFF** in the managed distro (`/etc/wsl.conf` or per-invocation) — this backend
//!   only *refuses* a network-on request; it does not disable a distro left network-on.
//! - **Automount OFF** (`/etc/wsl.conf` `[automount] enabled=false`) + an explicit bind of ONLY
//!   the work root. Default automount exposes every Windows drive read-write under `/mnt/*`, so
//!   sandboxed code could `cd /mnt/c/Users/<user>` and write outside the work root.

use super::config::{ExecOutput, ExecRequest, NetworkPolicy, SandboxConfig};
use super::sandbox::{Sandbox, SandboxKind};
use super::spawn::spawn_capture;
use super::SandboxError;
use async_trait::async_trait;
use std::path::Path;

/// A minimal, Linux-appropriate PATH for the child inside the distro (the parent's Windows
/// PATH is meaningless in WSL).
const WSL_INNER_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub struct Wsl2Sandbox {
    distro: String,
}

impl Wsl2Sandbox {
    pub fn new(distro: impl Into<String>) -> Self {
        Self { distro: distro.into() }
    }

    /// Discover the managed distro from `HAILY_WSL_DISTRO`. `None` → no enforcing WSL backend
    /// (Manager falls back to `NullSandbox`). We never auto-select the user's default distro.
    pub fn detect() -> Option<Self> {
        std::env::var("HAILY_WSL_DISTRO")
            .ok()
            .filter(|d| !d.is_empty())
            .map(Self::new)
    }

    pub fn distro(&self) -> &str {
        &self.distro
    }
}

/// Translate a Windows path (`C:\a\b`) to its WSL mount path (`/mnt/c/a/b`). Returns `None`
/// for a path without a drive letter (e.g. already-UNC or malformed).
pub fn win_path_to_wsl(win: &Path) -> Option<String> {
    let s = win.to_string_lossy().replace('\\', "/");
    let (drive, rest) = s.split_once(":/")?;
    let letter = drive.chars().next()?.to_ascii_lowercase();
    if !letter.is_ascii_alphabetic() || drive.len() != 1 {
        return None;
    }
    Some(format!("/mnt/{letter}/{rest}"))
}

/// Build the inner `env -i` pairs (Linux PATH + scratch HOME + locale + caller overrides).
fn wsl_inner_env(scratch_home: &str, extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = vec![
        ("PATH".to_string(), WSL_INNER_PATH.to_string()),
        ("HOME".to_string(), scratch_home.to_string()),
        ("TMPDIR".to_string(), scratch_home.to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
    ];
    env.extend(extra.iter().cloned());
    env
}

/// Assemble the full `wsl.exe` argv: run inside `distro`, cd to the mounted work root, wipe the
/// env with `env -i`, then exec the program. Pure — unit-tested without spawning.
pub fn build_wsl_argv(
    distro: &str,
    wsl_work_root: &str,
    program: &str,
    program_args: &[String],
    inner_env: &[(String, String)],
) -> Vec<String> {
    let mut argv = vec![
        "-d".to_string(),
        distro.to_string(),
        "--cd".to_string(),
        wsl_work_root.to_string(),
        "--".to_string(),
        "env".to_string(),
        "-i".to_string(),
    ];
    for (k, v) in inner_env {
        argv.push(format!("{k}={v}"));
    }
    argv.push(program.to_string());
    argv.extend(program_args.iter().cloned());
    argv
}

#[async_trait]
impl Sandbox for Wsl2Sandbox {
    fn kind(&self) -> SandboxKind {
        SandboxKind::Wsl2
    }

    fn is_enforcing(&self) -> bool {
        true
    }

    async fn exec(
        &self,
        req: ExecRequest,
        cfg: &SandboxConfig,
    ) -> Result<ExecOutput, SandboxError> {
        // Network-on requires a distinctly-provisioned network-on distro (P13 browser only);
        // the managed code-exec distro is network-off. Refuse a mismatch rather than run a
        // network-off distro pretending to satisfy a network-on request.
        if cfg.network == NetworkPolicy::On {
            return Err(SandboxError::BackendUnavailable(
                "network-on profile requires the P13 browser distro; code-exec distro is network-off"
                    .into(),
            ));
        }

        let wsl_root = win_path_to_wsl(&req.work_root).ok_or_else(|| {
            SandboxError::InvalidWorkRoot(req.work_root.clone())
        })?;
        let inner_env = wsl_inner_env(&wsl_root, &req.env_overrides);
        let argv = build_wsl_argv(&self.distro, &wsl_root, &req.program, &req.args, &inner_env);

        // The launcher env for wsl.exe itself only needs a (non-secret) PATH to be located.
        let launcher_env: Vec<(String, String)> = std::env::var("PATH")
            .ok()
            .map(|p| vec![("PATH".to_string(), p)])
            .unwrap_or_default();
        let timeout = req.timeout.unwrap_or(cfg.timeout);

        spawn_capture(
            "wsl.exe",
            &argv,
            &req.work_root,
            &launcher_env,
            timeout,
            SandboxKind::Wsl2,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win_path_translation() {
        assert_eq!(
            win_path_to_wsl(Path::new(r"C:\haily\work")).as_deref(),
            Some("/mnt/c/haily/work")
        );
        assert_eq!(
            win_path_to_wsl(Path::new(r"D:\a\b c")).as_deref(),
            Some("/mnt/d/a/b c")
        );
    }

    #[test]
    fn argv_isolates_env_and_targets_distro() {
        let env = wsl_inner_env("/mnt/c/work", &[("EXTRA".into(), "1".into())]);
        let argv = build_wsl_argv("haily-sandbox", "/mnt/c/work", "cargo", &["check".into()], &env);
        assert_eq!(argv[0], "-d");
        assert_eq!(argv[1], "haily-sandbox");
        assert!(argv.contains(&"--cd".to_string()));
        assert!(argv.contains(&"-i".to_string())); // env -i wipes inherited env
        assert!(argv.contains(&"cargo".to_string()));
        assert!(argv.contains(&"check".to_string()));
        assert!(argv.iter().any(|a| a == "EXTRA=1"));
        assert!(argv.iter().any(|a| a.starts_with("PATH=/usr")));
    }

    #[test]
    fn detect_absent_without_env() {
        std::env::remove_var("HAILY_WSL_DISTRO");
        assert!(Wsl2Sandbox::detect().is_none());
    }

    #[tokio::test]
    async fn network_on_is_refused_by_code_exec_distro() {
        let sb = Wsl2Sandbox::new("haily-sandbox");
        let dir = tempfile::tempdir().unwrap();
        let cfg = SandboxConfig {
            network: NetworkPolicy::On,
            ..Default::default()
        };
        let r = sb.exec(ExecRequest::new("true", dir.path()), &cfg).await;
        assert!(matches!(r, Err(SandboxError::BackendUnavailable(_))));
    }

    /// Live proof (opt-in): a real WSL exec. Requires a provisioned managed distro named in
    /// `HAILY_WSL_DISTRO` and `HAILY_WSL_SANDBOX_TEST=1`. Skipped in CI (matches the
    /// `HAILY_ODOO_URL` live-sandbox-skip idiom).
    #[tokio::test]
    async fn live_wsl_echo() {
        if std::env::var("HAILY_WSL_SANDBOX_TEST").is_err() {
            return;
        }
        let Some(sb) = Wsl2Sandbox::detect() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let out = sb
            .exec(
                ExecRequest::new("echo", dir.path()).arg("hello-from-wsl"),
                &SandboxConfig::default(),
            )
            .await
            .expect("live wsl exec");
        assert!(out.stdout.contains("hello-from-wsl"));
        assert_eq!(out.backend, SandboxKind::Wsl2);
    }
}
