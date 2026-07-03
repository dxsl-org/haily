use crate::{RiskTier, Tool, ToolContext};
use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::process::Command;

// ---------------------------------------------------------------------------
// WorktreeApplyTool
// ---------------------------------------------------------------------------
pub struct WorktreeApplyTool;

#[async_trait]
impl Tool for WorktreeApplyTool {
    fn name(&self) -> &str { "worktree_apply" }

    fn description(&self) -> &str {
        "Xem diff hoặc áp dụng các thay đổi từ ephemeral worktree vào workspace chính. \
         Dùng sau khi developer sub-agent thực hiện thay đổi trong sandbox."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "worktree_path": {
                    "type": "string",
                    "description": "Đường dẫn tuyệt đối tới ephemeral worktree"
                },
                "confirm": {
                    "type": "boolean",
                    "description": "false = chỉ xem diff; true = áp dụng thay đổi vào workspace chính",
                    "default": false
                }
            },
            "required": ["worktree_path"]
        })
    }

    fn risk_tier(&self, _args: &Value) -> RiskTier { RiskTier::IrreversibleWrite }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String> {
        let worktree_path = args["worktree_path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("worktree_path required"))?;
        let confirm = args["confirm"].as_bool().unwrap_or(false);

        let wt_path = PathBuf::from(worktree_path);
        if !wt_path.exists() {
            bail!("worktree path does not exist: {worktree_path}");
        }

        // Determine unified diff (tracked changes + untracked files).
        let diff = compute_diff(&wt_path).await?;

        if diff.is_empty() {
            return Ok("Worktree không có thay đổi nào.".to_string());
        }

        if !confirm {
            return Ok(format!(
                "📋 Diff preview:\n\n```diff\n{diff}\n```\n\nGọi lại với confirm=true để áp dụng."
            ));
        }

        // Resolve the main (non-linked) worktree root via `git worktree list`.
        // Using current_dir() would be wrong when the process runs from a
        // different directory or when the worktree belongs to a different repo.
        let repo_root = resolve_main_worktree(&wt_path).await?;

        // Collect modified tracked files.
        let tracked_output = Command::new("git")
            .args(["-C", worktree_path, "diff", "--name-only", "HEAD"])
            .output()
            .await?;

        if !tracked_output.status.success() {
            let stderr = String::from_utf8_lossy(&tracked_output.stderr);
            bail!("git diff --name-only HEAD failed: {stderr}");
        }
        let tracked_str = String::from_utf8_lossy(&tracked_output.stdout);

        // Collect untracked files.
        let untracked_output = Command::new("git")
            .args(["-C", worktree_path, "ls-files", "--others", "--exclude-standard"])
            .output()
            .await?;

        if !untracked_output.status.success() {
            let stderr = String::from_utf8_lossy(&untracked_output.stderr);
            bail!("git ls-files failed: {stderr}");
        }
        let untracked_str = String::from_utf8_lossy(&untracked_output.stdout);

        let all_rel_paths: Vec<&str> = tracked_str
            .lines()
            .chain(untracked_str.lines())
            .filter(|l| !l.is_empty())
            .collect();

        // Stage copies to a vec first so we can report all-or-nothing on error.
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();

        for rel_path in &all_rel_paths {
            // Reject paths with '..' components or absolute paths — git would
            // never emit these, but defend against a compromised worktree.
            crate::security::validate_rel_path(rel_path)?;

            let src = wt_path.join(rel_path);

            // Reject symlinks — following them could write outside the worktree.
            if crate::security::is_symlink(&src).await? {
                tracing::warn!(path = rel_path, "skipping symlink in worktree_apply");
                continue;
            }

            let dst = repo_root.join(rel_path);
            staged.push((src, dst));
        }

        // Apply all copies. On first failure the workspace may be partially
        // updated — this is logged but not rolled back (rollback requires a
        // staging area outside the live workspace).
        let mut applied: Vec<String> = Vec::new();
        for (src, dst) in &staged {
            if let Some(parent) = dst.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(src, dst).await?;
            applied.push(
                dst.strip_prefix(&repo_root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| dst.display().to_string()),
            );
        }

        // Remove the worktree registration and temp dir via git.
        let cleanup = Command::new("git")
            .args(["worktree", "remove", "--force", worktree_path])
            .current_dir(&repo_root)
            .output()
            .await?;

        if !cleanup.status.success() {
            let stderr = String::from_utf8_lossy(&cleanup.stderr);
            tracing::warn!("worktree cleanup failed: {stderr}");
        }

        let file_list = applied.iter().map(|f| format!("  • {f}")).collect::<Vec<_>>().join("\n");

        Ok(format!(
            "Đã áp dụng {count} file vào workspace:\n{file_list}",
            count = applied.len(),
        ))
    }
}

/// Resolve the main (non-linked) worktree root from any linked worktree path.
///
/// `git worktree list --porcelain` always lists the main worktree first.
async fn resolve_main_worktree(wt_path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(wt_path)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git worktree list failed: {stderr}");
    }

    // First "worktree <path>" line = main worktree (git guarantees this).
    let stdout = String::from_utf8_lossy(&output.stdout);
    let main_path = stdout
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot determine main worktree from git output"))?;

    Ok(main_path)
}

/// Produce a unified diff: tracked changes via `git diff HEAD` plus untracked
/// files inlined as pseudo-diff blocks. Returns an empty string when clean.
///
/// # Safety
/// Each untracked path is validated with [`crate::security::validate_rel_path`]
/// (rejects `..`/absolute paths) and checked with [`crate::security::is_symlink`]
/// before being read — a compromised worktree could otherwise report an untracked
/// "file" that is actually a symlink to something outside the worktree (e.g.
/// `/etc/shadow`), which `read_to_string` would happily follow. Both checks skip
/// (with a `warn` log) rather than failing the whole diff, matching `execute()`'s
/// per-file skip semantics for the apply path.
///
/// # Size caps
/// Each file is capped at [`crate::security::DIFF_MAX_FILE_BYTES`] and the total
/// diff at [`crate::security::DIFF_MAX_TOTAL_BYTES`] — an oversized untracked file
/// (e.g. an accidentally-committed binary or log dump) must not make the diff
/// preview unusable or blow up context when handed to an LLM. Both caps append
/// [`crate::security::TRUNCATED_MARKER`] so truncation is visible, not silent.
async fn compute_diff(wt_path: &PathBuf) -> Result<String> {
    use crate::security::{is_symlink, validate_rel_path, DIFF_MAX_FILE_BYTES, DIFF_MAX_TOTAL_BYTES, TRUNCATED_MARKER};

    let tracked = Command::new("git")
        .args(["-C"])
        .arg(wt_path)
        .args(["diff", "HEAD"])
        .output()
        .await?;

    if !tracked.status.success() {
        let stderr = String::from_utf8_lossy(&tracked.stderr);
        bail!("git diff HEAD failed: {stderr}");
    }

    let mut diff = String::from_utf8_lossy(&tracked.stdout).into_owned();
    if diff.len() >= DIFF_MAX_TOTAL_BYTES {
        diff.truncate(DIFF_MAX_TOTAL_BYTES);
        diff.push_str(TRUNCATED_MARKER);
        return Ok(diff);
    }

    let untracked = Command::new("git")
        .args(["-C"])
        .arg(wt_path)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .await?;

    if !untracked.status.success() {
        let stderr = String::from_utf8_lossy(&untracked.stderr);
        bail!("git ls-files failed: {stderr}");
    }

    let file_list = String::from_utf8_lossy(&untracked.stdout);
    for rel_path in file_list.lines() {
        if rel_path.is_empty() {
            continue;
        }
        if let Err(e) = validate_rel_path(rel_path) {
            tracing::warn!(path = rel_path, "skipping untracked file in diff: {e}");
            continue;
        }

        let abs_path = wt_path.join(rel_path);

        match is_symlink(&abs_path).await {
            Ok(true) => {
                tracing::warn!(path = rel_path, "skipping symlink in diff preview");
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(path = %abs_path.display(), "skipping untracked file in diff: {e}");
                continue;
            }
        }

        let raw = match tokio::fs::read(&abs_path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(path = %abs_path.display(), "skipping untracked file in diff: {e}");
                continue;
            }
        };

        let file_truncated = raw.len() > DIFF_MAX_FILE_BYTES;
        let capped = if file_truncated { &raw[..DIFF_MAX_FILE_BYTES] } else { &raw[..] };
        let contents = String::from_utf8_lossy(capped);

        let mut block = format!(
            "--- /dev/null\n+++ b/{rel_path}\n@@ -0,0 +1,{lines} @@\n",
            lines = contents.lines().count()
        );
        for line in contents.lines() {
            block.push('+');
            block.push_str(line);
            block.push('\n');
        }
        if file_truncated {
            block.push_str(TRUNCATED_MARKER);
        }

        if diff.len() + block.len() > DIFF_MAX_TOTAL_BYTES {
            diff.push_str(TRUNCATED_MARKER);
            return Ok(diff);
        }
        diff.push_str(&block);
    }

    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a minimal git repo with one empty commit so `HEAD` is valid — mirrors
    /// the fixture in `haily-core/tests/worktree.rs` (same shape, kept file-local so
    /// this crate doesn't take a dev-dependency on `haily-core`).
    fn init_git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();

        let init = std::process::Command::new("git").args(["init"]).current_dir(p).output().expect("git init");
        assert!(init.status.success(), "git init failed: {}", String::from_utf8_lossy(&init.stderr));

        let commit = std::process::Command::new("git")
            .args([
                "-c", "user.email=test@haily.test",
                "-c", "user.name=Test",
                "commit", "--allow-empty", "-m", "initial",
            ])
            .current_dir(p)
            .output()
            .expect("git commit");
        assert!(commit.status.success(), "initial commit failed: {}", String::from_utf8_lossy(&commit.stderr));

        dir
    }

    #[tokio::test]
    async fn compute_diff_includes_valid_untracked_file() {
        let repo = init_git_repo();
        tokio::fs::write(repo.path().join("hello.txt"), "world\n").await.unwrap();

        let diff = compute_diff(&repo.path().to_path_buf()).await.unwrap();
        assert!(diff.contains("hello.txt"));
        assert!(diff.contains("+world"));
    }

    #[tokio::test]
    async fn compute_diff_skips_path_traversal_escape() {
        // `validate_rel_path` only rejects paths it's handed — git itself would
        // never emit a `..`-bearing `ls-files` line, so this test exercises the
        // guard function directly on the value compute_diff would have received,
        // proving the guard (not git's own well-behavedness) is what blocks it.
        assert!(crate::security::validate_rel_path("../escape.txt").is_err());
    }

    #[tokio::test]
    async fn compute_diff_skips_absolute_path() {
        #[cfg(unix)]
        let abs = "/etc/passwd";
        #[cfg(windows)]
        let abs = "C:\\Windows\\System32\\config";
        assert!(crate::security::validate_rel_path(abs).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn compute_diff_skips_symlink_to_outside_but_keeps_valid_files() {
        use std::os::unix::fs::symlink;

        let repo = init_git_repo();
        let outside = tempfile::tempdir().unwrap();
        tokio::fs::write(outside.path().join("secret.txt"), "should not leak\n").await.unwrap();

        // A symlink inside the worktree pointing outside it, plus one normal file.
        symlink(outside.path().join("secret.txt"), repo.path().join("link.txt")).unwrap();
        tokio::fs::write(repo.path().join("real.txt"), "real content\n").await.unwrap();

        let diff = compute_diff(&repo.path().to_path_buf()).await.unwrap();
        assert!(diff.contains("real.txt"), "valid file must still be diffed:\n{diff}");
        assert!(diff.contains("real content"));
        assert!(!diff.contains("should not leak"), "symlink target content must never appear in the diff:\n{diff}");
    }

    #[tokio::test]
    async fn compute_diff_truncates_oversized_untracked_file() {
        let repo = init_git_repo();
        // One byte over the per-file cap.
        let oversized = "a".repeat(crate::security::DIFF_MAX_FILE_BYTES + 1);
        tokio::fs::write(repo.path().join("big.txt"), &oversized).await.unwrap();

        let diff = compute_diff(&repo.path().to_path_buf()).await.unwrap();
        assert!(diff.contains("big.txt"));
        assert!(diff.contains(crate::security::TRUNCATED_MARKER.trim()), "oversized file must carry the truncated marker:\n{}", &diff[..diff.len().min(200)]);
    }

    #[tokio::test]
    async fn compute_diff_empty_on_clean_worktree() {
        let repo = init_git_repo();
        let diff = compute_diff(&repo.path().to_path_buf()).await.unwrap();
        assert!(diff.is_empty());
    }
}
