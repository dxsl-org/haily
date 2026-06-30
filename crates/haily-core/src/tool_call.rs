/// Tool call parsing, loop-guard, and dispatch.
use anyhow::{bail, Result};
use haily_io::ResponseChunk;
use haily_tools::{ToolClass, ToolContext, ToolRegistry};
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

const MAX_TOOL_CALLS: u32 = 10;

/// Guards against runaway loops: identical consecutive calls and call-count ceiling.
pub struct LoopGuard {
    last: Option<(String, String)>, // (tool_name, args_json)
    count: u32,
}

impl LoopGuard {
    pub fn new() -> Self { Self { last: None, count: 0 } }

    /// Returns Err if the call is a duplicate or if the ceiling is reached.
    pub fn check(&mut self, tool: &str, args: &serde_json::Value) -> Result<()> {
        let args_str = args.to_string();
        if let Some((last_tool, last_args)) = &self.last {
            if last_tool == tool && *last_args == args_str {
                bail!("loop guard: identical call to '{tool}' repeated — stopping");
            }
        }
        if self.count >= MAX_TOOL_CALLS {
            bail!("loop guard: reached {MAX_TOOL_CALLS} tool calls in one turn — stopping");
        }
        self.last = Some((tool.to_string(), args_str));
        self.count += 1;
        Ok(())
    }
}

/// Extract the first `<tool_call>…</tool_call>` block from an LLM response.
/// Returns `(tool_name, args, text_before)` or None if no call present.
pub fn parse_tool_call(response: &str) -> Option<(String, serde_json::Value)> {
    let start = response.find("<tool_call>")?;
    let after_open = &response[start + "<tool_call>".len()..];
    let end = after_open.find("</tool_call>")?;
    let json_str = after_open[..end].trim();

    let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let tool = parsed["tool"].as_str()?.to_string();
    let args = parsed.get("args").cloned().unwrap_or(serde_json::Value::Object(Default::default()));
    Some((tool, args))
}

/// Strip all `<tool_call>` and `<tool_result>` blocks from text before sending to user.
pub fn strip_tool_markup(text: &str) -> String {
    let mut out = text.to_string();
    for (open, close) in [("<tool_call>", "</tool_call>"), ("<tool_result>", "</tool_result>")] {
        while let Some(start) = out.find(open) {
            if let Some(rel_end) = out[start..].find(close) {
                let end = start + rel_end + close.len();
                out.drain(start..end);
            } else {
                out.truncate(start);
                break;
            }
        }
    }
    out.trim().to_string()
}

/// Execute a parsed tool call: check guard, check class, run, send status chunk.
/// Returns the tool result string to inject as a tool_result message.
pub async fn dispatch(
    tool_name: &str,
    args: serde_json::Value,
    registry: &ToolRegistry,
    ctx: &ToolContext,
    tx: &mpsc::Sender<ResponseChunk>,
    guard: &mut LoopGuard,
) -> Result<String> {
    guard.check(tool_name, &args)?;

    let tool = registry
        .get(tool_name)
        .ok_or_else(|| anyhow::anyhow!("unknown tool '{tool_name}'"))?;

    match tool.approval_class() {
        ToolClass::Blocked => {
            bail!("tool '{tool_name}' is blocked");
        }
        ToolClass::RequireApproval => {
            let approval_id = Uuid::new_v4();
            // In V1: send approval request chunk, then auto-approve after 0ms.
            // Phase 10 (GUI) will add real approval UI by intercepting this chunk.
            let _ = tx
                .send(ResponseChunk::ToolApprovalRequest {
                    tool: tool_name.to_string(),
                    args: args.to_string(),
                    approval_id,
                })
                .await;
            // Auto-approve: continue immediately.
        }
        ToolClass::AutoApprove => {}
    }

    info!(tool = tool_name, "executing tool");
    let result = match tool.execute(args, ctx).await {
        Ok(output) => {
            let _ = tx.send(ResponseChunk::ToolResult { name: tool_name.to_string(), ok: true }).await;
            output
        }
        Err(e) => {
            warn!(tool = tool_name, error = %e, "tool failed");
            let _ = tx.send(ResponseChunk::ToolResult { name: tool_name.to_string(), ok: false }).await;
            format!("Tool error: {e:#}")
        }
    };

    Ok(result)
}
