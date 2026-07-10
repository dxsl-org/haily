//! Minimal git tools scoped to a workspace's worktree: `git_status`, `git_diff` (reads), and
//! `git_commit` (ReversibleWrite — the branch is workspace-local).
//!
//! git isolation: `git_commit` writes objects to an ISOLATED store (`GIT_OBJECT_DIRECTORY`)
//! sibling to the worktree, with the real repo's objects as a read-only alternate. New
//! commit/tree/blob objects therefore never enter the real repo's shared object DB, and
//! discarding the worktree removes them entirely (verified by `discard` teardown + the
//! integration test). No reflog/gc churn is inflicted on the user's real repo.

use super::workspace::{git, object_dir_for};
use super::{journal_coding_audit, load_workspace};
use crate::connector::redact;
use crate::{RiskTier, Tool, ToolContext};
use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

/// Cap on diff/status output returned to the model.
const MAX_GIT_OUTPUT: usize = 16_000;

fn cap(mut s: String) -> String {
    if s.len() > MAX_GIT_OUTPUT {
        let mut end = MAX_GIT_OUTPUT;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("\n[truncated]");
    }
    s
}

pub struct GitStatusTool;

#[async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }
    fn description(&self) -> &str {
        "Trạng thái git của workspace (git status --porcelain)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "workspace_id": { "type": "string" } },
            "required": ["workspace_id"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let ws = load_workspace(ctx, &args).await?;
        let root = Path::new(&ws.worktree_path);
        let out = git(root, &["status", "--porcelain"], &[]).await?;
        if !out.status.success() {
            bail!("git status failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        // Tag-strip: a filename in `git status` output could carry a `<tool_call>` token.
        let s = redact::strip_tool_tags(&String::from_utf8_lossy(&out.stdout));
        Ok(if s.trim().is_empty() {
            "clean (no changes)".to_string()
        } else {
            cap(s)
        })
    }
}

pub struct GitDiffTool;

#[async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }
    fn description(&self) -> &str {
        "Diff các thay đổi trong workspace (git diff, gồm cả staged nếu staged=true)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "staged": { "type": "boolean", "default": false }
            },
            "required": ["workspace_id"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let ws = load_workspace(ctx, &args).await?;
        let root = Path::new(&ws.worktree_path);
        let mut git_args = vec!["diff"];
        if args["staged"].as_bool().unwrap_or(false) {
            git_args.push("--staged");
        }
        let out = git(root, &git_args, &[]).await?;
        if !out.status.success() {
            bail!("git diff failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        // Tag-strip: diff hunks are attacker-controlled source content and may embed a
        // `<tool_call>` token that would otherwise steer the fix loop.
        let s = redact::strip_tool_tags(&String::from_utf8_lossy(&out.stdout));
        Ok(if s.trim().is_empty() {
            "no diff".to_string()
        } else {
            cap(s)
        })
    }
}

pub struct GitCommitTool;

#[async_trait]
impl Tool for GitCommitTool {
    fn name(&self) -> &str {
        "git_commit"
    }
    fn description(&self) -> &str {
        "Commit tất cả thay đổi trong workspace vào branch riêng của workspace (object store \
         cô lập, không đụng repo thật)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "message": { "type": "string" }
            },
            "required": ["workspace_id", "message"]
        })
    }
    /// `ReversibleWrite`: the commit lands on the workspace-local branch in an isolated object
    /// store, fully discarded with the worktree. Kill-switch-gated like any non-Read tool.
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let ws = load_workspace(ctx, &args).await?;
        let root = Path::new(&ws.worktree_path);
        let repo = Path::new(&ws.repo_path);
        let message = args["message"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("message (string) is required"))?;

        // Resolve the real repo's objects dir as a READ-ONLY alternate, and create the
        // isolated write target so new objects never enter the real store.
        let alt_out = git(repo, &["rev-parse", "--git-path", "objects"], &[]).await?;
        if !alt_out.status.success() {
            bail!("cannot resolve repo objects dir: {}", String::from_utf8_lossy(&alt_out.stderr));
        }
        let alt_rel = String::from_utf8_lossy(&alt_out.stdout).trim().to_string();
        // `--git-path` yields a path relative to the repo cwd; make it absolute.
        let alt_abs = repo.join(&alt_rel);
        let obj_dir = object_dir_for(&ws.worktree_path);
        tokio::fs::create_dir_all(&obj_dir).await?;

        let env = [
            ("GIT_OBJECT_DIRECTORY", obj_dir.as_str()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alt_abs.to_str().unwrap_or_default()),
        ];

        let add = git(root, &["add", "-A"], &env).await?;
        if !add.status.success() {
            bail!("git add failed: {}", String::from_utf8_lossy(&add.stderr));
        }
        // Ephemeral-workspace identity so a commit never fails on missing user config.
        let commit = git(
            root,
            &[
                "-c",
                "user.name=Haily",
                "-c",
                "user.email=haily@localhost",
                "commit",
                "-m",
                message,
            ],
            &env,
        )
        .await?;
        if !commit.status.success() {
            let stderr = String::from_utf8_lossy(&commit.stderr);
            let stdout = String::from_utf8_lossy(&commit.stdout);
            bail!("git commit failed: {stderr}{stdout}");
        }
        journal_coding_audit(ctx, &ws.id, "git_commit", "commit", &ws.branch).await?;
        let stdout = redact::strip_tool_tags(&String::from_utf8_lossy(&commit.stdout));
        Ok(format!("Committed to {}:\n{}", ws.branch, stdout.trim()))
    }
}
