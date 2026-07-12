//! `fs_move` and `fs_delete` — first-class `ReversibleWrite` file operations (red-team M4:
//! without them a refactor/rename falls to `shell_exec mv`, which is unlisted → approval →
//! wedges a headless run). Reverted by the worktree compensator, kill-switch-gated (non-Read),
//! but DELIBERATELY NOT in `RETIERED_DELETE_TOOLS`: that per-turn cap is for DB-row soft-deletes
//! whose undo is a row restore; coding deletes live in an ephemeral worktree fully reverted by
//! `git clean -ffdx`, and capping them at 5 would wedge a legitimate multi-file refactor.

use super::path_guard::{canonical_root, is_git_hook_path, is_secret_path, resolve_in_workspace};
use super::{is_git_internal, journal_coding_audit, load_workspace};
use crate::{RiskTier, Tool, ToolContext};
use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

/// Reject a mutation target: secret-matched, `.git` control path, or a git hook.
fn guard_mutate_path(rel: &str) -> Result<()> {
    if is_secret_path(rel) {
        bail!("refusing to move/delete a secret-matched path: {rel}");
    }
    if is_git_internal(rel) {
        bail!("refusing to touch the .git control directory: {rel}");
    }
    if is_git_hook_path(rel) {
        bail!("refusing to touch a git hook: {rel}");
    }
    Ok(())
}

pub struct FsMoveTool;

#[async_trait]
impl Tool for FsMoveTool {
    fn name(&self) -> &str {
        "fs_move"
    }
    fn description(&self) -> &str {
        "Di chuyển/đổi tên file trong workspace (cả nguồn và đích phải nằm trong workspace)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "from": { "type": "string" },
                "to": { "type": "string" }
            },
            "required": ["workspace_id", "from", "to"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let ws = load_workspace(ctx, &args).await?;
        let root = canonical_root(Path::new(&ws.worktree_path))?;
        let from = args["from"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("from (string) is required"))?;
        let to = args["to"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("to (string) is required"))?;
        guard_mutate_path(from)?;
        guard_mutate_path(to)?;
        let from_abs = resolve_in_workspace(&root, from)?;
        let to_abs = resolve_in_workspace(&root, to)?;
        if !from_abs.exists() {
            bail!("source does not exist: {from}");
        }
        if let Some(parent) = to_abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::rename(&from_abs, &to_abs).await?;
        journal_coding_audit(ctx, &ws.id, "fs_move", "move", &format!("{from} -> {to}")).await?;
        Ok(format!("Moved {from} -> {to}"))
    }
}

pub struct FsDeleteTool;

#[async_trait]
impl Tool for FsDeleteTool {
    fn name(&self) -> &str {
        "fs_delete"
    }
    fn description(&self) -> &str {
        "Xóa file hoặc thư mục trong workspace. Có thể hoàn tác bằng cách reset worktree."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "path": { "type": "string" }
            },
            "required": ["workspace_id", "path"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let ws = load_workspace(ctx, &args).await?;
        let root = canonical_root(Path::new(&ws.worktree_path))?;
        let rel = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("path (string) is required"))?;
        guard_mutate_path(rel)?;
        let abs = resolve_in_workspace(&root, rel)?;
        if abs == root {
            bail!("refusing to delete the workspace root");
        }
        let meta = tokio::fs::symlink_metadata(&abs).await?;
        if meta.file_type().is_dir() {
            tokio::fs::remove_dir_all(&abs).await?;
        } else {
            tokio::fs::remove_file(&abs).await?;
        }
        journal_coding_audit(ctx, &ws.id, "fs_delete", "delete", rel).await?;
        Ok(format!("Deleted {rel}"))
    }
}
