//! Isolated-object-store git reads for the Build Pipeline wrapper.
//!
//! The runner commits each passed stage into the workspace's ISOLATED object store
//! (`GIT_OBJECT_DIRECTORY`), so a plain `git diff` from `haily-core` would not resolve those
//! objects. These helpers replicate the workspace's env pair (primary = isolated dir, alternate
//! = the real repo's objects) so the wrapper can read committed state to (a) inject the phase
//! diff into the Review prompt and (b) feed the Fix-round delta to the reward-hacking guard.
//!
//! Best-effort by contract: git being unavailable or a rev missing yields an empty result, never
//! an error that would abort a run — a review with an empty diff is degraded, not broken.

use std::path::Path;

use haily_tools::coding::workspace::CodingWorkspace;
use tokio::process::Command;

/// The `(GIT_OBJECT_DIRECTORY, GIT_ALTERNATE_OBJECT_DIRECTORIES)` env pair for `ws`, matching
/// the workspace's own commit/compensate env (primary isolated store + the real repo's objects
/// as the read-through alternate). `None` if the alternate cannot be resolved.
async fn isolated_env(ws: &CodingWorkspace) -> Option<(String, String)> {
    let repo = ws.row.repo_path.clone();
    let out = Command::new("git")
        .args(["-C", &repo, "rev-parse", "--git-path", "objects"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let alt_rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let alt_abs = Path::new(&repo)
        .join(alt_rel)
        .to_string_lossy()
        .into_owned();
    let obj = ws.object_dir().to_string_lossy().into_owned();
    Some((obj, alt_abs))
}

/// Current HEAD sha of the workspace worktree (the "phase base" captured before a build), or
/// `None` if it cannot be read.
pub async fn head_sha(ws: &CodingWorkspace) -> Option<String> {
    let env = isolated_env(ws).await?;
    let out = Command::new("git")
        .args(["-C", &ws.row.worktree_path, "rev-parse", "HEAD"])
        .env("GIT_OBJECT_DIRECTORY", &env.0)
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &env.1)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Unified diff of committed changes from `base`..HEAD in the workspace's isolated object store.
/// Empty string when `base` is `None`/unreadable, HEAD equals base, or git errors — a degraded
/// (empty) diff never aborts the review.
pub async fn diff_since(ws: &CodingWorkspace, base: Option<&str>) -> String {
    let Some(base) = base else {
        return String::new();
    };
    let Some(env) = isolated_env(ws).await else {
        return String::new();
    };
    let out = Command::new("git")
        .args(["-C", &ws.row.worktree_path, "diff", base, "HEAD"])
        .env("GIT_OBJECT_DIRECTORY", &env.0)
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &env.1)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(),
    }
}
