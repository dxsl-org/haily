use crate::{Tool, ToolClass, ToolContext};
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

    fn approval_class(&self) -> ToolClass { ToolClass::RequireApproval }

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
            let rel = Path::new(rel_path);
            if rel.is_absolute()
                || rel.components().any(|c| c == std::path::Component::ParentDir)
            {
                bail!("path traversal detected in worktree output: {rel_path}");
            }

            let src = wt_path.join(rel_path);

            // Reject symlinks — following them could write outside the worktree.
            let src_meta = tokio::fs::symlink_metadata(&src).await?;
            if src_meta.file_type().is_symlink() {
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
async fn compute_diff(wt_path: &PathBuf) -> Result<String> {
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
        let abs_path = wt_path.join(rel_path);
        let contents = match tokio::fs::read_to_string(&abs_path).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %abs_path.display(), "skipping untracked file in diff: {e}");
                continue;
            }
        };
        diff.push_str(&format!(
            "--- /dev/null\n+++ b/{rel_path}\n@@ -0,0 +1,{lines} @@\n",
            lines = contents.lines().count()
        ));
        for line in contents.lines() {
            diff.push('+');
            diff.push_str(line);
            diff.push('\n');
        }
    }

    Ok(diff)
}
