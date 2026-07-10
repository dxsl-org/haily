//! Shared child-process spawn + capture + timeout, used by every backend that ultimately
//! runs a real OS process (`NullSandbox` post-approval, `Wsl2Sandbox`, native backends).
//!
//! Contract: env is ALWAYS cleared then rebuilt from the caller's allowlisted pairs — a
//! backend must never inherit the parent env. Output is capped per stream; a kill-on-drop
//! child guarantees no orphan survives a cancelled/awaited-out future.

use super::config::{ExecOutput, MAX_OUTPUT_BYTES};
use super::{SandboxError, SandboxKind};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Spawn `program` with `args` in `work_root`, with EXACTLY `env` (parent env cleared first),
/// capturing stdout/stderr (capped) under a wall-clock `timeout`. `backend` is stamped onto
/// the result for attribution.
pub(crate) async fn spawn_capture(
    program: &str,
    args: &[String],
    work_root: &Path,
    env: &[(String, String)],
    timeout: Duration,
    backend: SandboxKind,
) -> Result<ExecOutput, SandboxError> {
    if !work_root.is_dir() {
        return Err(SandboxError::InvalidWorkRoot(work_root.to_path_buf()));
    }

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(work_root)
        .env_clear()
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().map_err(SandboxError::Spawn)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Drain both streams CONCURRENTLY with the wait, capping accumulation at MAX_OUTPUT_BYTES
    // as we read (never buffering the whole stream first) — attacker code that spews unbounded
    // output can no longer grow the host process's memory. We keep reading to EOF past the cap
    // so a full pipe never blocks (and thus stalls) the child.
    let run = async {
        let (out, err, status) = tokio::join!(read_capped(stdout), read_capped(stderr), child.wait());
        let status = status.map_err(SandboxError::Spawn)?;
        Ok::<_, SandboxError>((out, err, status))
    };
    let ((stdout, t1), (stderr, t2), status) = match tokio::time::timeout(timeout, run).await {
        Ok(res) => res?,
        // Dropping `run` drops `child`; `kill_on_drop(true)` guarantees no orphan survives.
        Err(_) => return Err(SandboxError::Timeout(timeout)),
    };

    Ok(ExecOutput {
        status: status.code().unwrap_or(-1),
        stdout,
        stderr,
        truncated: t1 || t2,
        backend,
    })
}

/// Read a child stream, accumulating at most [`MAX_OUTPUT_BYTES`] into memory but continuing to
/// drain to EOF (discarding the overflow) so the child never blocks on a full pipe. Returns the
/// lossy-decoded text and whether anything was dropped. Bounds host memory to the cap + one chunk.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>) -> (String, bool) {
    let Some(mut r) = reader else {
        return (String::new(), false);
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < MAX_OUTPUT_BYTES {
                    let take = (MAX_OUTPUT_BYTES - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true; // over the cap: keep draining, discard the bytes
                }
            }
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_env(dir: &Path) -> Vec<(String, String)> {
        // Include PATH so a program that lives outside the default CreateProcess search dirs
        // (e.g. powershell.exe) can be located; env_clear() would otherwise drop it.
        let mut env = vec![("HOME".into(), dir.to_string_lossy().into_owned())];
        if let Ok(path) = std::env::var("PATH") {
            env.push(("PATH".into(), path));
        }
        env
    }

    #[tokio::test]
    async fn invalid_work_root_errors() {
        let env = scratch_env(Path::new("/nonexistent"));
        let r = spawn_capture(
            "cmd",
            &[],
            Path::new("/definitely/not/a/dir/xyz"),
            &env,
            Duration::from_secs(5),
            SandboxKind::Null,
        )
        .await;
        assert!(matches!(r, Err(SandboxError::InvalidWorkRoot(_))));
    }

    #[tokio::test]
    async fn captures_exit_code_and_stdout() {
        let dir = tempfile::tempdir().unwrap();
        // Cross-platform tiny program: use the OS shell to echo + exit 0.
        #[cfg(windows)]
        let (prog, args) = ("cmd", vec!["/C".to_string(), "echo hello".to_string()]);
        #[cfg(not(windows))]
        let (prog, args) = ("sh", vec!["-c".to_string(), "echo hello".to_string()]);

        let out = spawn_capture(
            prog,
            &args,
            dir.path(),
            &scratch_env(dir.path()),
            Duration::from_secs(30),
            SandboxKind::Null,
        )
        .await
        .expect("spawn ok");
        assert_eq!(out.status, 0);
        assert!(out.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn read_capped_bounds_memory_and_flags_truncation() {
        // tokio implements AsyncRead for &[u8].
        let big = vec![b'A'; MAX_OUTPUT_BYTES + 50_000];
        let (text, truncated) = read_capped(Some(&big[..])).await;
        assert!(truncated, "over-cap output must be flagged truncated");
        assert_eq!(text.len(), MAX_OUTPUT_BYTES, "accumulation must be bounded at the cap");
    }

    #[tokio::test]
    async fn read_capped_small_stream_intact() {
        let (text, truncated) = read_capped(Some(&b"hello"[..])).await;
        assert_eq!(text, "hello");
        assert!(!truncated);

        let (empty, t) = read_capped(None::<&[u8]>).await;
        assert!(empty.is_empty());
        assert!(!t);
    }

    #[tokio::test]
    async fn timeout_trips() {
        let dir = tempfile::tempdir().unwrap();
        // A genuine multi-second block using only always-present executables. Loopback ping
        // returns instantly, and `powershell` fails under env_clear, so use `cmd` pinging an
        // unreachable private IP with a per-echo wait (~3s total) — well over the 200ms timeout.
        #[cfg(windows)]
        let (prog, args) = (
            "cmd",
            vec![
                "/C".to_string(),
                "ping -n 3 -w 1000 10.255.255.1 >nul".to_string(),
            ],
        );
        #[cfg(not(windows))]
        let (prog, args) = ("sh", vec!["-c".to_string(), "sleep 5".to_string()]);

        let r = spawn_capture(
            prog,
            &args,
            dir.path(),
            &scratch_env(dir.path()),
            Duration::from_millis(200),
            SandboxKind::Null,
        )
        .await;
        assert!(matches!(r, Err(SandboxError::Timeout(_))));
    }
}
