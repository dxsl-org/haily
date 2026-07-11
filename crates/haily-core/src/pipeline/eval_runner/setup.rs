//! Throwaway-workspace setup + gate execution for the coding eval (Sub-Agent + Skill
//! Architecture phase 9).
//!
//! Every eval run operates on a COPY of the fixture (copy-per-run), never the committed original
//! under `evals/fixtures/`, so a run can never mutate the source of truth — the
//! `no_out_of_workspace_writes` gate asserts the original is byte-unchanged afterward.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use haily_tools::exec::build_child_env;

/// Timeout for a scoring gate command (mirrors the runner's `GATE_TIMEOUT_SECS`).
const GATE_EXEC_TIMEOUT: Duration = Duration::from_secs(300);

/// Recursively copy `src` into a fresh temp dir and `git init` + commit it, returning the
/// throwaway repo path (the "real repo" a [`CodingWorkspace`] cuts its worktree from). The
/// caller owns the returned [`tempfile::TempDir`] and drops it to clean up.
///
/// # Errors
/// Returns an error if the copy or any git step fails.
pub async fn stage_throwaway_repo(src: &Path) -> Result<(tempfile::TempDir, PathBuf)> {
    let holder = tempfile::tempdir().context("creating throwaway repo dir")?;
    let repo = holder.path().join("repo");
    copy_dir_recursive(src, &repo).await.context("copying fixture into throwaway repo")?;

    git(&repo, &["init", "-b", "main"]).await?;
    git(&repo, &["config", "user.email", "eval@haily.test"]).await?;
    git(&repo, &["config", "user.name", "Haily Eval"]).await?;
    git(&repo, &["add", "-A"]).await?;
    git(&repo, &["commit", "-m", "eval fixture baseline"]).await?;
    Ok((holder, repo))
}

/// Run the fixture's own gate command against `worktree_root`, returning its exit code
/// (`None` = the verifier program is not installed — AD-M3, scored as a non-pass). Runs the
/// program directly (developer-authored, trusted) with the env allowlist + a scratch HOME, the
/// same honesty as the runner's non-enforcing gate branch.
///
/// # Errors
/// Returns an error only for a spawn/wait failure that is not "program not found".
pub async fn run_gate_command(
    program: &str,
    args: &[String],
    worktree_root: &Path,
) -> Result<Option<i32>> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(worktree_root)
        .env_clear()
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in build_child_env(worktree_root, &[]) {
        cmd.env(k, v);
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let out = tokio::time::timeout(GATE_EXEC_TIMEOUT, child.wait_with_output())
        .await
        .context("eval gate command timed out")??;
    Ok(Some(out.status.code().unwrap_or(-1)))
}

/// A cheap deterministic hash of a directory tree's relative paths + file bytes (excluding
/// `.git`), for the copy-per-run `no_out_of_workspace_writes` invariant: hash the original
/// before and after a run and assert equality.
///
/// # Errors
/// Returns an error if the directory cannot be walked.
pub async fn tree_hash(root: &Path) -> Result<String> {
    use std::hash::{Hash, Hasher};
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    collect_files(root, root, &mut entries).await?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (rel, bytes) in &entries {
        rel.hash(&mut h);
        bytes.hash(&mut h);
    }
    Ok(format!("{:016x}", h.finish()))
}

async fn collect_files(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    let mut rd = tokio::fs::read_dir(dir).await.context("reading dir")?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let ft = entry.file_type().await?;
        if ft.is_dir() {
            Box::pin(collect_files(base, &path, out)).await?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = tokio::fs::read(&path).await.unwrap_or_default();
            out.push((rel, bytes));
        }
    }
    Ok(())
}

async fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dst).await?;
    let mut rd = tokio::fs::read_dir(src).await.context("reading source dir")?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let name = entry.file_name();
        // Never carry a fixture's own `.git` (fixtures are committed under the repo's git, not
        // their own) or build artifacts into the throwaway.
        if name == ".git" || name == "target" || name == "node_modules" || name == "dist" {
            continue;
        }
        let dst_path = dst.join(&name);
        let ft = entry.file_type().await?;
        if ft.is_dir() {
            Box::pin(copy_dir_recursive(&path, &dst_path)).await?;
        } else if ft.is_file() {
            tokio::fs::copy(&path, &dst_path).await?;
        }
    }
    Ok(())
}

async fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .context("spawning git")?;
    if !out.status.success() {
        bail!("git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}
