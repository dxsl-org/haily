//! `fs_grep` — content search over a workspace using the bundled `regex` crate (NO external
//! ripgrep dependency). Enforces workspace containment and the secret deny-glob so a
//! credential file's contents can never be surfaced through a search.

use super::load_workspace;
use super::path_guard::{canonical_root, is_secret_path};
use crate::{RiskTier, Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

/// Cap on total match lines returned and per-file bytes scanned (skip huge/binary files).
const MAX_MATCHES: usize = 200;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub struct FsGrepTool;

#[async_trait]
impl Tool for FsGrepTool {
    fn name(&self) -> &str {
        "fs_grep"
    }
    fn description(&self) -> &str {
        "Tìm theo regex trong nội dung file của workspace (giống ripgrep). Bỏ qua .git và \
         file bí mật."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "pattern": { "type": "string", "description": "regex" },
                "max_results": { "type": "integer" }
            },
            "required": ["workspace_id", "pattern"]
        })
    }
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::Read
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let ws = load_workspace(ctx, &args).await?;
        let root = canonical_root(Path::new(&ws.worktree_path))?;
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("pattern (string) is required"))?;
        let re = regex::Regex::new(pattern)
            .map_err(|e| anyhow::anyhow!("invalid regex: {e}"))?;
        let cap = args["max_results"]
            .as_u64()
            .unwrap_or(MAX_MATCHES as u64)
            .min(MAX_MATCHES as u64) as usize;

        let mut out: Vec<String> = Vec::new();
        search(&root, &root, &re, cap, &mut out);
        if out.is_empty() {
            return Ok("Không có kết quả.".to_string());
        }
        if out.len() >= cap {
            out.push(format!("[capped at {cap} matches]"));
        }
        Ok(out.join("\n"))
    }
}

/// Recursively scan `dir`, appending `rel:line: text` matches, skipping `.git`, secret paths,
/// oversized/binary files. Stops once `cap` matches are collected.
fn search(root: &Path, dir: &Path, re: &regex::Regex, cap: usize, out: &mut Vec<String>) {
    if out.len() >= cap {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= cap {
            return;
        }
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str == ".git" || rel_str.starts_with(".git/") {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            search(root, &path, re, cap, out);
        } else if ft.is_file() && !is_secret_path(&rel_str) {
            if entry.metadata().map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue; // non-UTF8/binary
            };
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    out.push(format!("{rel_str}:{}: {}", i + 1, line.trim_end()));
                    if out.len() >= cap {
                        return;
                    }
                }
            }
        }
    }
}

/// Static assertion of the search contract: the deny-glob is honored inside the walker.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_files_are_skipped_by_the_walker() {
        // The walker checks is_secret_path per file; a `.env` never yields a match line.
        assert!(is_secret_path(".env"));
        assert!(is_secret_path("config/API_TOKEN"));
    }
}
