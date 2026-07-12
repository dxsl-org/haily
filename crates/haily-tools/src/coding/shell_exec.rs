//! `shell_exec` — run a command inside a CodingWorkspace, under the P0 sandbox.
//!
//! Verifier commands (build/test/lint) auto-run ONLY when the selected sandbox is enforcing
//! (isolation is the containment for attacker-authored `build.rs`/proc-macros/npm scripts);
//! otherwise they route through a first-exec-per-workspace approval. Non-verifier commands are
//! `IrreversibleWrite` (dispatch approves before `execute`). Cancellation: the kill switch's
//! `ctx.cancel` stops the in-flight child, not merely the next dispatch.

use super::{
    format_exec_result, journal_coding_audit, load_workspace, request_exec_approval,
    session_sandbox, shell_policy,
};
use crate::exec::sandbox::{cargo_safe_args, detect_redirection_triggers, git_safe_args};
use crate::exec::{build_child_env, spawn_capture_cancellable, ExecRequest, SandboxConfig, SandboxKind};
use crate::{RiskTier, Tool, ToolContext};
use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

/// Default per-command wall-clock ceiling, and the hard maximum a caller may request.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
const MAX_TIMEOUT_SECS: u64 = 600;

pub struct ShellExecTool;

/// Parse the `args` array into program args (dropping non-string entries).
fn command_args(args: &Value) -> Vec<String> {
    args["args"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

#[async_trait]
impl Tool for ShellExecTool {
    fn name(&self) -> &str {
        "shell_exec"
    }
    fn description(&self) -> &str {
        "Chạy một lệnh (build/test/lint hoặc lệnh khác) trong workspace coding, bên trong \
         sandbox. Verifier (cargo/npm/pytest/go test…) tự chạy khi sandbox cô lập được; \
         lệnh khác cần phê duyệt."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": { "type": "string" },
                "program": { "type": "string", "description": "Executable, e.g. cargo" },
                "args": { "type": "array", "items": { "type": "string" } },
                "timeout_secs": { "type": "integer", "description": "Wall-clock cap (default 300, max 600)" }
            },
            "required": ["workspace_id", "program"]
        })
    }

    /// Verifier → `ReversibleWrite` (may auto-run when isolated); everything else →
    /// `IrreversibleWrite` (approval). Fail-closed: a missing/non-string `program` cannot be
    /// classified, so it is `IrreversibleWrite`.
    fn risk_tier(&self, args: &Value) -> RiskTier {
        let Some(program) = args["program"].as_str() else {
            return RiskTier::IrreversibleWrite;
        };
        if shell_policy::is_verifier(program, &command_args(args)) {
            RiskTier::ReversibleWrite
        } else {
            RiskTier::IrreversibleWrite
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let ws = load_workspace(ctx, &args).await?;
        let root = Path::new(&ws.worktree_path).to_path_buf();
        if !root.is_dir() {
            bail!("workspace worktree missing on disk: {}", ws.worktree_path);
        }
        let program = args["program"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("program (string) is required"))?
            .to_string();
        let cmd_args = command_args(&args);
        let timeout = Duration::from_secs(
            args["timeout_secs"]
                .as_u64()
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .min(MAX_TIMEOUT_SECS),
        );

        // Prepend config-redirection defenses (SEC-C2): pin cargo's runner/wrapper and
        // neutralize any in-tree git hooks path. `_hooks_guard` keeps the empty hooks dir
        // alive for the exec duration.
        let mut full_args: Vec<String> = Vec::new();
        let mut _hooks_guard: Option<tempfile::TempDir> = None;
        if shell_policy::is_cargo(&program) {
            full_args.extend(cargo_safe_args());
        }
        if shell_policy::is_git(&program) {
            let hooks = tempfile::tempdir()?;
            full_args.extend(git_safe_args(hooks.path()));
            _hooks_guard = Some(hooks);
        }
        full_args.extend(cmd_args.iter().cloned());

        let triggers = detect_redirection_triggers(&root);
        let sb = session_sandbox(ctx);
        let enforcing = sb.is_enforcing();
        let verifier = shell_policy::is_verifier(&program, &cmd_args);
        // A user-supplied `--config`/`-c`/`--target-dir` comes AFTER our prepended safe-args and
        // would win, defeating the pinned trusted-runner / empty-hooksPath (SEC-C2). Route such a
        // verifier through a human even under an enforcing sandbox — never silently auto-run a
        // command that neutralizes the config-redirection pin.
        let overrides_pin = (shell_policy::is_cargo(&program) || shell_policy::is_git(&program))
            && cmd_args.iter().any(|a| {
                a == "--config"
                    || a == "-c"
                    || a.starts_with("--config=")
                    || a == "--target-dir"
                    || a.starts_with("--target-dir=")
            });

        // A verifier normally auto-runs (ReversibleWrite, no dispatch prompt) — but must route
        // through a human when it cannot be contained (non-enforcing sandbox), when a
        // config-redirection vector is present (SEC-C2), or when user args override the pin. A
        // non-verifier is IrreversibleWrite, already approved by dispatch before `execute`.
        if verifier && (!enforcing || !triggers.is_empty() || overrides_pin) {
            let reason = if !enforcing {
                "sandbox is not enforcing (first-exec approval)"
            } else if overrides_pin {
                "user args override the pinned config (--config/-c/--target-dir)"
            } else {
                "config-redirection vectors present"
            };
            let summary = format!(
                "shell_exec `{program} {}` in workspace {} — {reason}; triggers={:?}",
                cmd_args.join(" "),
                ws.id,
                triggers
            );
            if !request_exec_approval(ctx, "shell_exec", summary).await {
                bail!("command not approved by user");
            }
        }

        let output = if enforcing {
            // Full isolation via the sandbox. Cancellation via select-drop: firing `ctx.cancel`
            // drops the `exec` future, whose child (`kill_on_drop(true)`) is killed in flight.
            let mut req = ExecRequest::new(program.clone(), root.clone()).args(full_args.clone());
            req.timeout = Some(timeout);
            let cfg = SandboxConfig::default();
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => bail!("command cancelled by kill switch"),
                r = sb.exec(req, &cfg) => r?,
            }
        } else {
            // Non-enforcing: approved above (verifier) or by dispatch (non-verifier). Run
            // directly with the env allowlist + scratch HOME = work root — honest non-isolated
            // execution, fully cancellable via `ctx.cancel`.
            let env = build_child_env(&root, &[]);
            spawn_capture_cancellable(
                &program,
                &full_args,
                &root,
                &env,
                timeout,
                SandboxKind::Null,
                &ctx.cancel,
            )
            .await?
        };

        // Journal an audit row (display/grouping; worktree is the compensator).
        let op = format!("{program} {}", cmd_args.join(" "));
        journal_coding_audit(ctx, &ws.id, "shell_exec", &op, &ws.branch).await?;

        Ok(format_exec_result(&output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_tier_verifier_is_reversible_else_irreversible() {
        let t = ShellExecTool;
        assert_eq!(
            t.risk_tier(&json!({"program": "cargo", "args": ["test"]})),
            RiskTier::ReversibleWrite
        );
        assert_eq!(
            t.risk_tier(&json!({"program": "rm", "args": ["-rf", "x"]})),
            RiskTier::IrreversibleWrite
        );
    }

    #[test]
    fn risk_tier_fail_closed_on_missing_program() {
        let t = ShellExecTool;
        assert_eq!(t.risk_tier(&json!({})), RiskTier::IrreversibleWrite);
    }
}
