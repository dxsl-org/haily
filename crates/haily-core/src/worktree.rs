use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use uuid::Uuid;

/// Synchronous RAII guard that removes the worktree on drop (including panics).
///
/// Runs `git worktree remove --force` via the blocking stdlib `Command` so it
/// works inside `Drop`, which cannot be async. Set `disarmed = true` after a
/// successful async cleanup to skip the redundant sync call.
struct WorktreeDropGuard {
    path: PathBuf,
    repo_root: PathBuf,
    disarmed: bool,
}

impl Drop for WorktreeDropGuard {
    fn drop(&mut self) {
        if self.disarmed || !self.path.exists() {
            return;
        }
        let _ = std::process::Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .current_dir(&self.repo_root)
            .output();
    }
}

/// Isolated git worktree for safe agent file operations.
///
/// Creates a detached checkout at HEAD in a temp dir. The object store is
/// shared with the main repo so creation is cheap. All mutations inside the
/// worktree never affect the main workspace until explicitly applied.
pub struct EphemeralWorktree {
    /// Absolute path to the temporary worktree checkout.
    pub path: PathBuf,
    repo_root: PathBuf,
}

impl EphemeralWorktree {
    /// Create a detached worktree in a temp dir at HEAD.
    ///
    /// Returns `Err` if `repo_root` is not a git repository or git fails.
    pub async fn new(repo_root: &Path) -> Result<Self> {
        // Guard: .git must exist before touching git commands.
        let git_dir = repo_root.join(".git");
        if !git_dir.exists() {
            bail!("not a git repository: {}", repo_root.display());
        }

        let dir_name = format!("haily-wt-{}", Uuid::new_v4().simple());
        let path = std::env::temp_dir().join(&dir_name);

        let output = Command::new("git")
            .args(["worktree", "add", "--detach"])
            .arg(&path)
            .current_dir(repo_root)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree add failed: {stderr}");
        }

        Ok(Self {
            path,
            repo_root: repo_root.to_path_buf(),
        })
    }

    /// Capture unified diff of all changes vs HEAD, including untracked files.
    /// Returns an empty string if the worktree is clean.
    pub async fn diff(&self) -> Result<String> {
        // Tracked changes: git diff HEAD
        let tracked = Command::new("git")
            .args(["-C"])
            .arg(&self.path)
            .args(["diff", "HEAD"])
            .output()
            .await?;

        if !tracked.status.success() {
            let stderr = String::from_utf8_lossy(&tracked.stderr);
            bail!("git diff HEAD failed: {stderr}");
        }

        let mut diff = String::from_utf8_lossy(&tracked.stdout).into_owned();

        // Untracked files: list then inline each as a pseudo-diff block.
        let untracked = Command::new("git")
            .args(["-C"])
            .arg(&self.path)
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
            // Harden the untracked-file read the same way the worktree tool's
            // `compute_diff` does (Phase 9): git can list a path with `..`/absolute
            // components or a symlink pointing outside the worktree, and reading it
            // blindly would pull arbitrary repo/host files into the diff. Skip such
            // entries, and cap sizes so a huge untracked file can't OOM the diff.
            if haily_tools::security::validate_rel_path(rel_path).is_err() {
                tracing::warn!(%rel_path, "skipping untracked file with an unsafe path in diff");
                continue;
            }
            let abs_path = self.path.join(rel_path);
            if haily_tools::security::is_symlink(&abs_path)
                .await
                .unwrap_or(true)
            {
                tracing::warn!(path = %abs_path.display(), "skipping symlinked untracked file in diff");
                continue;
            }
            // Read file; skip on I/O error — binary or permission-denied files
            // should not abort the entire diff.
            let mut contents = match tokio::fs::read_to_string(&abs_path).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(path = %abs_path.display(), "skipping untracked file in diff: {e}");
                    continue;
                }
            };
            if contents.len() > haily_tools::security::DIFF_MAX_FILE_BYTES {
                let mut cut = haily_tools::security::DIFF_MAX_FILE_BYTES;
                while !contents.is_char_boundary(cut) {
                    cut -= 1;
                }
                contents.truncate(cut);
                contents.push_str(haily_tools::security::TRUNCATED_MARKER);
            }
            // Emit a minimal unified-diff-style header so callers can parse it.
            diff.push_str(&format!(
                "--- /dev/null\n+++ b/{rel_path}\n@@ -0,0 +1,{lines} @@\n",
                lines = contents.lines().count()
            ));
            for line in contents.lines() {
                diff.push('+');
                diff.push_str(line);
                diff.push('\n');
            }
            if diff.len() > haily_tools::security::DIFF_MAX_TOTAL_BYTES {
                diff.push_str(haily_tools::security::TRUNCATED_MARKER);
                tracing::warn!("worktree diff exceeded total size cap — truncated");
                break;
            }
        }

        Ok(diff)
    }

    /// Remove the worktree registration and delete the temp dir.
    /// Idempotent — safe to call multiple times.
    pub async fn cleanup(&self) -> Result<()> {
        // Skip if the directory no longer exists (already cleaned up).
        if !self.path.exists() {
            return Ok(());
        }

        let output = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .current_dir(&self.repo_root)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree remove failed: {stderr}");
        }

        Ok(())
    }

    /// Run `f` in an isolated worktree and guarantee cleanup even on panic.
    ///
    /// The closure receives the absolute path to the worktree checkout.
    /// Cleanup runs on both normal exit and panic: a synchronous `Drop` guard
    /// handles the panic case; async cleanup runs after `f` returns normally.
    /// If both `f` and async cleanup fail, `f`'s error is returned and the
    /// cleanup error is logged as a warning.
    ///
    /// Returns `(result_of_f, diff_string)`.
    pub async fn with_ephemeral_worktree<F, Fut, T>(repo_root: &Path, f: F) -> Result<(T, String)>
    where
        F: FnOnce(PathBuf) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T>> + Send,
        T: Send,
    {
        let wt = EphemeralWorktree::new(repo_root).await?;

        // Sync drop guard: fires on panic or task-cancellation before the
        // async cleanup below has a chance to run.
        let mut guard = WorktreeDropGuard {
            path: wt.path.clone(),
            repo_root: wt.repo_root.clone(),
            disarmed: false,
        };

        // Run the caller's function; capture result before cleanup so we can
        // take the diff while changes are still present.
        let user_result = f(wt.path.clone()).await;

        // Capture diff before cleanup (changes gone after remove).
        let diff = match wt.diff().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("failed to capture worktree diff: {e:#}");
                String::new()
            }
        };

        // Async cleanup: preferred path. Disarm the sync guard so Drop is a no-op.
        if let Err(e) = wt.cleanup().await {
            tracing::warn!("worktree cleanup failed: {e:#}");
        }
        guard.disarmed = true;

        let value = user_result?;
        Ok((value, diff))
    }
}
