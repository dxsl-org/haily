//! `code_exec` — the domain-agnostic "run a snippet to solve a problem" tool.
//!
//! Writes a generated snippet to a THROWAWAY scratch root and runs it under the SAME P0
//! sandbox as coding's `shell_exec` — harness-first: no domain (researcher/finance/explorer)
//! gets un-sandboxed execution. Per-domain enablement is a `sub_registry` whitelist decision,
//! not a new code path. Inherits the exact isolation + approval + tag-strip contract:
//! auto-runs only when the sandbox is enforcing; a non-enforcing sandbox routes through
//! first-exec approval; output is capped + tag-stripped; the kill switch (`ctx.cancel`) stops
//! an in-flight run.

use super::config::{build_child_env, ExecRequest, SandboxConfig};
use super::sandbox::SandboxKind;
use super::spawn_capture_cancellable;
use crate::coding::{format_exec_result, request_exec_approval, session_sandbox};
use crate::{RiskTier, Tool, ToolContext};
use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

pub struct CodeExecTool;

/// Map a language to `(interpreter, snippet filename)`. `None` → unsupported.
fn runner(language: &str) -> Option<(&'static str, &'static str)> {
    match language.to_ascii_lowercase().as_str() {
        "python" | "py" => Some(("python", "snippet.py")),
        "bash" | "sh" => Some(("bash", "snippet.sh")),
        "node" | "javascript" | "js" => Some(("node", "snippet.js")),
        "ruby" | "rb" => Some(("ruby", "snippet.rb")),
        _ => None,
    }
}

#[async_trait]
impl Tool for CodeExecTool {
    fn name(&self) -> &str {
        "code_exec"
    }
    fn description(&self) -> &str {
        "Chạy một đoạn code ngắn (python/bash/node/ruby) trong sandbox cô lập để tính toán/giải \
         quyết vấn đề. Không mạng, thư mục scratch tạm."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "language": { "type": "string", "enum": ["python", "bash", "node", "ruby"] },
                "code": { "type": "string" },
                "timeout_secs": { "type": "integer", "description": "default 120, max 600" }
            },
            "required": ["language", "code"]
        })
    }
    /// `ReversibleWrite`: the snippet runs in a throwaway scratch root with no persistent side
    /// effect. May auto-run only when the sandbox is enforcing (checked in `execute`);
    /// otherwise `execute` requires first-exec approval. Kill-switch-gated like any non-Read.
    fn risk_tier(&self, _args: &Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let language = args["language"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("language (string) is required"))?;
        let code = args["code"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("code (string) is required"))?;
        let (program, filename) =
            runner(language).ok_or_else(|| anyhow::anyhow!("unsupported language: {language}"))?;
        let timeout = Duration::from_secs(
            args["timeout_secs"]
                .as_u64()
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .min(MAX_TIMEOUT_SECS),
        );

        let scratch = tempfile::tempdir()?;
        let snippet = scratch.path().join(filename);
        tokio::fs::write(&snippet, code).await?;

        let sb = session_sandbox(ctx);
        let enforcing = sb.is_enforcing();
        if !enforcing {
            let summary = format!("code_exec ({language}) — sandbox is not enforcing; first-exec approval");
            if !request_exec_approval(ctx, "code_exec", summary).await {
                bail!("code execution not approved by user");
            }
        }

        let file_arg = filename.to_string();
        let output = if enforcing {
            let mut req = ExecRequest::new(program, scratch.path()).arg(&file_arg);
            req.timeout = Some(timeout);
            let cfg = SandboxConfig::default();
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => bail!("code_exec cancelled by kill switch"),
                r = sb.exec(req, &cfg) => r?,
            }
        } else {
            let env = build_child_env(scratch.path(), &[]);
            spawn_capture_cancellable(
                program,
                &[file_arg],
                scratch.path(),
                &env,
                timeout,
                SandboxKind::Null,
                &ctx.cancel,
            )
            .await?
        };

        Ok(format_exec_result(&output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_maps_known_languages() {
        assert_eq!(runner("python"), Some(("python", "snippet.py")));
        assert_eq!(runner("JS"), Some(("node", "snippet.js")));
        assert!(runner("cobol").is_none());
    }

    #[test]
    fn risk_tier_is_reversible() {
        assert_eq!(CodeExecTool.risk_tier(&json!({})), RiskTier::ReversibleWrite);
    }
}
