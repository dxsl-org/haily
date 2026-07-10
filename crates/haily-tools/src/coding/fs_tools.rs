//! Read-only file tools: `fs_read` (line-numbered, offset/limit) and `fs_list` (glob).
//! Both enforce workspace containment and the secret deny-glob on every path they touch.

use super::path_guard::{canonical_root, is_secret_path, resolve_in_workspace};
use super::{content_hash, load_workspace};
use crate::{RiskTier, Tool, ToolContext};
use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

/// Cap on lines returned by a single `fs_read` window and files listed by `fs_list`.
const MAX_READ_LINES: usize = 2_000;
const MAX_LIST_ENTRIES: usize = 1_000;

pub struct FsReadTool;

#[async_trait]
impl Tool for FsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn description(&self) -> &str {
        "Đọc một file trong workspace (đánh số dòng, hỗ trợ offset/limit). Trả về cả content \
         hash để dùng cho fs_edit."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "path": { "type": "string" },
                "offset": { "type": "integer", "description": "1-based first line (default 1)" },
                "limit": { "type": "integer", "description": "max lines (default 2000)" }
            },
            "required": ["workspace_id", "path"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let ws = load_workspace(ctx, &args).await?;
        let root = canonical_root(Path::new(&ws.worktree_path))?;
        let rel = args["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("path (string) is required"))?;
        if is_secret_path(rel) {
            bail!("refusing to read a secret-matched path: {rel}");
        }
        let abs = resolve_in_workspace(&root, rel)?;
        // Re-check the RESOLVED path: an in-repo symlink (e.g. `notes.txt -> secret.env`, target
        // still inside root so containment passes) would otherwise defeat the request-path check,
        // since `resolve_in_workspace` canonicalizes through the link to the real target.
        if let Ok(resolved_rel) = abs.strip_prefix(&root) {
            if is_secret_path(&resolved_rel.to_string_lossy()) {
                bail!("refusing to read a secret-matched path (resolved through a link): {rel}");
            }
        }
        let text = tokio::fs::read_to_string(&abs).await?;
        let hash = content_hash(&text);

        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().unwrap_or(MAX_READ_LINES as u64) as usize;
        let limit = limit.min(MAX_READ_LINES);

        let mut out = format!("path: {rel}\ncontent_hash: {hash}\n");
        let lines: Vec<&str> = text.lines().collect();
        let start = offset - 1;
        for (i, line) in lines.iter().enumerate().skip(start).take(limit) {
            out.push_str(&format!("{:>6}\t{line}\n", i + 1));
        }
        if start >= lines.len() && !lines.is_empty() {
            bail!("offset {offset} is past end of file ({} lines)", lines.len());
        }
        Ok(out)
    }
}

pub struct FsListTool;

#[async_trait]
impl Tool for FsListTool {
    fn name(&self) -> &str {
        "fs_list"
    }
    fn description(&self) -> &str {
        "Liệt kê file trong workspace, lọc theo glob (vd '**/*.rs'). Bỏ qua .git và file bí mật."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "pattern": { "type": "string", "description": "glob, e.g. '**/*.rs' (optional)" }
            },
            "required": ["workspace_id"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let ws = load_workspace(ctx, &args).await?;
        let root = canonical_root(Path::new(&ws.worktree_path))?;
        let pattern = args["pattern"].as_str();
        let re = pattern.map(glob_to_regex).transpose()?;

        let mut found: Vec<String> = Vec::new();
        walk(&root, &root, &re, &mut found);
        found.sort();
        found.truncate(MAX_LIST_ENTRIES);
        if found.is_empty() {
            return Ok("Không có file nào khớp.".to_string());
        }
        Ok(found.join("\n"))
    }
}

/// Recursively collect workspace-relative file paths, skipping `.git` and secret-matched
/// paths, applying the optional glob (as a compiled regex).
fn walk(root: &Path, dir: &Path, re: &Option<regex::Regex>, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str == ".git" || rel_str.starts_with(".git/") {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            walk(root, &path, re, out);
        } else if file_type.is_file()
            && !is_secret_path(&rel_str)
            && re.as_ref().map(|r| r.is_match(&rel_str)).unwrap_or(true)
        {
            out.push(rel_str);
        }
    }
}

/// Translate a glob (`*`, `?`, `**`) into an anchored regex. `**` matches across `/`; `*`
/// does not. Bundled — no external glob crate.
fn glob_to_regex(glob: &str) -> Result<regex::Regex> {
    let mut re = String::from("^");
    let bytes = glob.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'*' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    re.push_str(".*");
                    i += 1;
                } else {
                    re.push_str("[^/]*");
                }
            }
            b'?' => re.push_str("[^/]"),
            c => {
                let ch = c as char;
                if ".+()|[]{}^$\\".contains(ch) {
                    re.push('\\');
                }
                re.push(ch);
            }
        }
        i += 1;
    }
    re.push('$');
    regex::Regex::new(&re).map_err(|e| anyhow::anyhow!("invalid glob '{glob}': {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_to_regex_matches_expected() {
        let r = glob_to_regex("**/*.rs").unwrap();
        assert!(r.is_match("src/main.rs"));
        assert!(r.is_match("a/b/c.rs"));
        assert!(!r.is_match("src/main.py"));

        let single = glob_to_regex("*.rs").unwrap();
        assert!(single.is_match("main.rs"));
        assert!(!single.is_match("src/main.rs")); // * does not cross '/'

        let q = glob_to_regex("file?.txt").unwrap();
        assert!(q.is_match("file1.txt"));
        assert!(!q.is_match("file12.txt"));
    }
}
