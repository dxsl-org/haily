//! Coding tool surface, every tool inside the Safe Operator harness.
//!
//! All file/search/shell/git tools are scoped to a [`workspace::CodingWorkspace`] — a git
//! worktree of a target repo. No write ever escapes the workspace root (see
//! [`path_guard`]); shell/code execution runs inside the P0 [`crate::exec`] sandbox; risk
//! tiers are fail-closed. Coding changes are journaled as AUDIT rows (display/grouping) —
//! the worktree, not the journal, is the authoritative compensator (a coding undo is
//! `git checkout -- . && git clean -ffdx`, per [`workspace::CodingWorkspace::discard`]).

pub mod fs_edit;
pub mod fs_mutate;
pub mod fs_tools;
pub mod git_tools;
pub mod grep_tool;
pub mod lint_on_edit;
pub mod path_guard;
pub mod shell_exec;
pub mod shell_policy;
pub mod stack_detect;
pub mod workspace;

pub use fs_edit::{FsEditTool, FsWriteTool};
pub use fs_mutate::{FsDeleteTool, FsMoveTool};
pub use fs_tools::{FsListTool, FsReadTool};
pub use git_tools::{GitCommitTool, GitDiffTool, GitStatusTool};
pub use grep_tool::FsGrepTool;
pub use shell_exec::ShellExecTool;

use crate::connector::redact;
use crate::exec::{ExecOutput, Manager, ScopeKey, Sandbox};
use crate::ToolContext;
use anyhow::{anyhow, Result};
use haily_db::queries::coding_workspaces::{self, CodingWorkspaceRow};
use haily_db::queries::journal::{self, NewAction};
use haily_types::ResponseChunk;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

/// Char budget for a shell/code exec result returned to the model. Bounds context growth
/// independently of the 1 MiB stream cap the sandbox already applies. When exceeded, the
/// middle is elided (head + tail preserved) so the decisive first errors AND the final
/// summary line both survive.
const EXEC_RESULT_CHAR_CAP: usize = 8_000;
const EXEC_HEAD_LINES: usize = 80;
const EXEC_TAIL_LINES: usize = 40;

/// The retention window for a coding audit row — shares the local-tool policy.
const CODING_RETENTION_DAYS: i64 = crate::LOCAL_RETENTION_DAYS;

/// Process-wide sandbox pool for coding execution. `ToolContext` cannot carry a `Manager`
/// without widening ~20 construction sites across `haily-core`/`haily-proactive`, so the
/// coding surface owns one lazily-initialized pool keyed by session scope — the same
/// isolation contract, scoped exactly as the Manager intends (one sandbox reused across a
/// session's turns). Backend selection + fail-safe-to-`NullSandbox` are the Manager's.
static CODING_SANDBOX_MANAGER: OnceLock<Manager> = OnceLock::new();

/// Acquire (or create) the pooled sandbox for `ctx`'s session.
pub(crate) fn session_sandbox(ctx: &ToolContext) -> Arc<dyn Sandbox> {
    CODING_SANDBOX_MANAGER
        .get_or_init(Manager::default)
        .get(ScopeKey::session(ctx.session_id.to_string()))
}

/// Load the caller's `workspace_id` argument and resolve it to an active workspace row,
/// SCOPED to the session (a workspace id parsed from LLM/tool text can never reach another
/// session's worktree — mirrors `journal::get_by_id_scoped`).
///
/// # Errors
/// Returns an error if `workspace_id` is missing/non-string, or does not name an active
/// workspace in this session.
pub(crate) async fn load_workspace(
    ctx: &ToolContext,
    args: &Value,
) -> Result<CodingWorkspaceRow> {
    let workspace_id = args["workspace_id"]
        .as_str()
        .ok_or_else(|| anyhow!("workspace_id (string) is required"))?;
    coding_workspaces::get_scoped(&ctx.db, workspace_id, &ctx.session_id.to_string())
        .await?
        .ok_or_else(|| anyhow!("no active workspace '{workspace_id}' in this session"))
}

/// Record a lightweight AUDIT row for a coding file mutation and stamp `ctx.last_journal_id`
/// so dispatch surfaces it as a reversible `ToolResult`. This row is display/grouping only:
/// the worktree compensator (`git checkout -- . && git clean -ffdx`) reverts the bytes, so
/// no file content is snapshotted here (red-team FMA-C2: one compensator over the bytes).
///
/// `op` is a short verb (`write`/`edit`/`move`/`delete`); `rel_path` is the
/// workspace-relative target. `request_params` is redacted before it lands on disk.
///
/// # Errors
/// Returns an error if the audit insert fails.
pub(crate) async fn journal_coding_audit(
    ctx: &ToolContext,
    workspace_id: &str,
    tool_name: &str,
    op: &str,
    rel_path: &str,
) -> Result<()> {
    let request = redact::redact_to_string(json!({ "op": op, "path": rel_path }), "coding");
    let idem = uuid::Uuid::new_v4().to_string();
    let turn = ctx.turn_id.to_string();
    let row = journal::insert_coding_audit(
        &ctx.db,
        NewAction {
            session_id: &ctx.session_id.to_string(),
            tool_name,
            tool_tier: "ReversibleWrite",
            // Reverted by the worktree compensator, not a per-row plan.
            compensability: "reversible",
            idempotency_key: &idem,
            correlation_ref: rel_path,
            request_params: &request,
            pre_state: None,
            pre_state_version: None,
            compensation_plan: None,
            turn_id: Some(&turn),
            retention_days: CODING_RETENTION_DAYS,
            manifest_hash: None,
        },
        workspace_id,
        // P4b: the active pipeline run id for in-pipeline coding writes (set by the runner on
        // the stage sub-turn's `ToolContext`); an ad-hoc coding sub-turn outside a run has
        // `run_id == None`, leaving the column NULL.
        ctx.run_id.as_deref(),
    )
    .await?;
    match ctx.last_journal_id.lock() {
        Ok(mut g) => *g = Some(row.id.clone()),
        Err(poisoned) => *poisoned.into_inner() = Some(row.id.clone()),
    }
    Ok(())
}

/// Render a completed sandbox exec into a concise, model-safe result string: exit code,
/// then stdout/stderr each capped (head + tail, middle elided) and tag-stripped so no
/// `<tool_call>` token in attacker-controlled compiler output can steer the fix loop
/// (SWE-agent "no silent output" + red-team untrusted-diagnostics rule).
pub(crate) fn format_exec_result(out: &ExecOutput) -> String {
    let mut body = String::new();
    body.push_str(&format!("exit_code: {}\n", out.status));
    if out.truncated {
        body.push_str("[stream truncated at sandbox cap]\n");
    }
    if !out.stdout.trim().is_empty() {
        body.push_str("--- stdout ---\n");
        body.push_str(&cap_text(&out.stdout));
        body.push('\n');
    }
    if !out.stderr.trim().is_empty() {
        body.push_str("--- stderr ---\n");
        body.push_str(&cap_text(&out.stderr));
        body.push('\n');
    }
    redact::strip_tool_tags(&body)
}

/// Head+tail line cap for one stream. Preserves the leading lines (first errors) and the
/// trailing lines (final summary) when a stream is longer than the budget.
fn cap_text(text: &str) -> String {
    if text.len() <= EXEC_RESULT_CHAR_CAP {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= EXEC_HEAD_LINES + EXEC_TAIL_LINES {
        // Long but few lines (e.g. one huge line): hard char cut at a boundary.
        let mut end = EXEC_RESULT_CHAR_CAP;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        return format!("{}\n[... truncated ...]", &text[..end]);
    }
    let head = lines[..EXEC_HEAD_LINES].join("\n");
    let tail = lines[lines.len() - EXEC_TAIL_LINES..].join("\n");
    let elided = lines.len() - EXEC_HEAD_LINES - EXEC_TAIL_LINES;
    format!("{head}\n[... {elided} lines elided ...]\n{tail}")
}

/// Deterministic content hash used to anchor a `fs_edit` against a stale read (FMA-M3 root
/// fix). Not cryptographic — staleness detection only. `fs_read` returns this so the model
/// can pass it back as `expected_hash`; a mismatch means the file changed since the read.
pub(crate) fn content_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    // DefaultHasher uses fixed keys → deterministic across runs, unlike RandomState.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Reject a mutation targeting the `.git` control directory (case-folded) — writing/moving/
/// deleting inside `.git` would corrupt the worktree or plant a config-redirection vector.
pub(crate) fn is_git_internal(rel: &str) -> bool {
    let lower = rel.replace('\\', "/").to_ascii_lowercase();
    lower == ".git" || lower.starts_with(".git/")
}

/// Raise a tool-approval prompt from INSIDE a tool's `execute` (used by the exec tools when a
/// verifier cannot auto-run: a non-enforcing sandbox → first-exec-per-workspace approval, or
/// a config-redirection trigger present). Mirrors dispatch's approval flow but is initiated by
/// the tool rather than the tier gate, because the tier is `ReversibleWrite` (auto-run) yet the
/// runtime condition (no isolation / redirection vector) demands a human. Returns `true` iff
/// approved. Awaits `ctx.cancel` so a shutdown never wedges the prompt.
pub(crate) async fn request_exec_approval(
    ctx: &ToolContext,
    tool_name: &str,
    summary: String,
) -> bool {
    let approval_id = Uuid::new_v4();
    let _ = ctx
        .approval_tx
        .send(ResponseChunk::ToolApprovalRequest {
            tool: tool_name.to_string(),
            args: summary,
            approval_id,
            origin: None,
            reversible: false,
        })
        .await;
    ctx.approval_gate
        .request(approval_id, ctx.session_id, &ctx.cancel)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::SandboxKind;

    fn out(status: i32, stdout: &str, stderr: &str, truncated: bool) -> ExecOutput {
        ExecOutput {
            status,
            stdout: stdout.into(),
            stderr: stderr.into(),
            truncated,
            backend: SandboxKind::Null,
        }
    }

    #[test]
    fn format_exec_result_reports_exit_and_strips_tags() {
        let o = out(0, "ok <tool_call>{}</tool_call> done", "", false);
        let s = format_exec_result(&o);
        assert!(s.contains("exit_code: 0"));
        assert!(!s.contains("<tool_call>"), "tags must be stripped: {s}");
        assert!(s.contains("done"));
    }

    #[test]
    fn cap_text_elides_middle_of_a_long_stream() {
        let many: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let capped = cap_text(&many);
        assert!(capped.contains("line 0"), "head preserved");
        assert!(capped.contains("line 999"), "tail preserved");
        assert!(capped.contains("elided"), "middle elided marker present");
        assert!(capped.len() < many.len());
    }

    #[test]
    fn cap_text_leaves_short_output_intact() {
        assert_eq!(cap_text("short output"), "short output");
    }
}
