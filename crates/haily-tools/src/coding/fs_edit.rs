//! Mutating file tools: `fs_write` (create/overwrite) and `fs_edit` (hash-anchored,
//! unique exact-match replace). Both are `ReversibleWrite` (the worktree compensator reverts
//! them), run lint-on-edit before persisting, and reject any path outside the workspace, any
//! secret-matched path, and any `.git` control-dir write.

use super::path_guard::{canonical_root, is_git_hook_path, is_secret_path, resolve_in_workspace};
use super::{content_hash, is_git_internal, journal_coding_audit, lint_on_edit, load_workspace};
use crate::{RiskTier, Tool, ToolContext};
use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

/// Reject a write/edit target that must never be written: outside root (handled by
/// `resolve_in_workspace`), a secret-matched path, a `.git` control path, or a git hook.
fn guard_write_path(rel: &str) -> Result<()> {
    if is_secret_path(rel) {
        bail!("refusing to write a secret-matched path: {rel}");
    }
    if is_git_internal(rel) {
        bail!("refusing to write inside the .git control directory: {rel}");
    }
    if is_git_hook_path(rel) {
        bail!("refusing to write a git hook (config-redirection vector): {rel}");
    }
    Ok(())
}

pub struct FsWriteTool;

#[async_trait]
impl Tool for FsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }
    fn description(&self) -> &str {
        "Tạo hoặc ghi đè một file trong workspace. Kiểm tra cú pháp trước khi ghi."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["workspace_id", "path", "content"]
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
        let content = args["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("content (string) is required"))?;
        guard_write_path(rel)?;
        let abs = resolve_in_workspace(&root, rel)?;

        lint_on_edit::check(rel, content)?;

        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&abs, content).await?;
        journal_coding_audit(ctx, &ws.id, "fs_write", "write", rel).await?;
        Ok(format!("Wrote {} bytes to {rel}", content.len()))
    }
}

pub struct FsEditTool;

#[async_trait]
impl Tool for FsEditTool {
    fn name(&self) -> &str {
        "fs_edit"
    }
    fn description(&self) -> &str {
        "Sửa file bằng cách thay thế một đoạn text khớp DUY NHẤT (exact match). Truyền \
         expected_hash (từ fs_read) để chống sửa trên bản đã cũ."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "path": { "type": "string" },
                "old_str": { "type": "string", "description": "text to replace (must be unique)" },
                "new_str": { "type": "string" },
                "expected_hash": { "type": "string", "description": "content_hash from fs_read (optional anti-stale anchor)" }
            },
            "required": ["workspace_id", "path", "old_str", "new_str"]
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
        let old_str = args["old_str"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("old_str (string) is required"))?;
        let new_str = args["new_str"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("new_str (string) is required"))?;
        guard_write_path(rel)?;
        let abs = resolve_in_workspace(&root, rel)?;

        let content = tokio::fs::read_to_string(&abs).await?;

        // Hash-anchor (FMA-M3 root fix): if the caller pinned the hash it read, a mismatch
        // means the file changed since — refuse cleanly rather than edit stale content.
        if let Some(expected) = args["expected_hash"].as_str() {
            let actual = content_hash(&content);
            if expected != actual {
                bail!(
                    "stale edit: file changed since read (expected_hash {expected}, actual {actual}) — re-read and retry"
                );
            }
        }

        let (new_content, how) = apply_edit(&content, old_str, new_str)?;
        lint_on_edit::check(rel, &new_content)?;
        tokio::fs::write(&abs, &new_content).await?;
        journal_coding_audit(ctx, &ws.id, "fs_edit", "edit", rel).await?;
        Ok(format!("Edited {rel} ({how})"))
    }
}

/// Apply a UNIQUE exact-match replace, with a bounded whitespace-normalization fallback that
/// STILL requires a unique match (no fuzzy/first-match — that would break the exact-unique
/// safety contract). Returns the new content and a description of which tier matched.
///
/// Idempotency (FMA-M3): re-applying the same edit finds 0 matches (already replaced) and
/// fails cleanly, never re-editing.
fn apply_edit(content: &str, old: &str, new: &str) -> Result<(String, &'static str)> {
    if old.is_empty() {
        bail!("old_str must not be empty");
    }
    // 1. Exact.
    match content.matches(old).count() {
        1 => return Ok((content.replacen(old, new, 1), "exact")),
        n if n > 1 => bail!("{n} matches for old_str; it must be UNIQUE — add surrounding context"),
        _ => {}
    }
    // 2. CRLF-normalized exact (Windows line endings).
    let content_lf = content.replace("\r\n", "\n");
    let old_lf = old.replace("\r\n", "\n");
    let new_lf = new.replace("\r\n", "\n");
    match content_lf.matches(&old_lf).count() {
        1 => return Ok((content_lf.replacen(&old_lf, &new_lf, 1), "newline-normalized")),
        n if n > 1 => bail!("{n} matches after newline normalization; must be UNIQUE"),
        _ => {}
    }
    // 3. Trailing-whitespace-normalized (per line), still requiring uniqueness.
    let norm = |s: &str| s.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
    let content_ws = norm(&content_lf);
    let old_ws = norm(&old_lf);
    match content_ws.matches(&old_ws).count() {
        1 => {
            let replaced = content_ws.replacen(&old_ws, &norm(&new_lf), 1);
            return Ok((replaced, "whitespace-normalized"));
        }
        n if n > 1 => bail!("{n} whitespace-normalized matches; must be UNIQUE"),
        _ => {}
    }
    bail!("no match for old_str in {}.{}", "file", nearest_hint(content, old));
}

/// Nearest-line "did you mean" diagnostics for a failed edit — helps a weak model self-correct
/// without exposing a fuzzy-apply path.
fn nearest_hint(content: &str, old: &str) -> String {
    let target = old.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if target.is_empty() {
        return String::new();
    }
    let needle = &target[..target.len().min(16)];
    let mut hits = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.contains(needle) {
            hits.push(format!("  line {}: {}", i + 1, line.trim()));
            if hits.len() >= 3 {
                break;
            }
        }
    }
    if hits.is_empty() {
        " (no similar line found)".to_string()
    } else {
        format!(" did you mean:\n{}", hits.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_unique_match_replaces() {
        let (out, how) = apply_edit("let x = 1;\nlet y = 2;\n", "let x = 1;", "let x = 42;").unwrap();
        assert!(out.contains("let x = 42;"));
        assert_eq!(how, "exact");
    }

    #[test]
    fn zero_match_errors_with_hint() {
        let err = apply_edit("let x = 1;\n", "let z = 9;", "q").unwrap_err();
        assert!(format!("{err}").contains("no match"), "{err}");
    }

    #[test]
    fn multi_match_errors() {
        let err = apply_edit("a\na\n", "a", "b").unwrap_err();
        assert!(format!("{err}").contains("must be UNIQUE"), "{err}");
    }

    #[test]
    fn idempotent_reapply_fails_cleanly() {
        // First edit succeeds; re-applying the SAME edit finds 0 matches (already changed).
        let (out, _) = apply_edit("v = OLD", "OLD", "NEW").unwrap();
        assert_eq!(out, "v = NEW");
        assert!(apply_edit(&out, "OLD", "NEW").is_err(), "stale re-apply must fail");
    }

    #[test]
    fn whitespace_normalized_match_when_trailing_ws_differs() {
        // Content has trailing spaces the model's old_str omits — still a unique match.
        let content = "fn a() {   \n  body\n}\n";
        let (out, how) = apply_edit(content, "fn a() {\n  body\n}", "fn a() {\n  new\n}").unwrap();
        assert!(out.contains("new"));
        assert_eq!(how, "whitespace-normalized");
    }
}
