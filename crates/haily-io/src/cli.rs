use crate::{Adapter, Notification, Request, RequestSender, ResponseChunk, WorkItemStatus};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct CliAdapter {
    /// Active work items cached by notify(WorkItemsChanged). Read before each prompt.
    /// Only the REPL loop writes to stdout, so updating this from notify() is race-free.
    status: Arc<Mutex<Vec<WorkItemStatus>>>,
    /// Cancelled when the stdin reader hits EOF (Ctrl+D) or a fatal read error, so the
    /// app entry point can treat "input stream closed" as a shutdown request.
    eof: CancellationToken,
}

impl CliAdapter {
    pub fn new() -> Self {
        Self { status: Arc::new(Mutex::new(Vec::new())), eof: CancellationToken::new() }
    }

    /// A token that fires when stdin closes (Ctrl+D). The entry point races this
    /// alongside OS signals so Ctrl+D quits cleanly instead of leaving an idle process.
    pub fn eof_token(&self) -> CancellationToken {
        self.eof.clone()
    }
}

impl Default for CliAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for CliAdapter {
    /// Reads stdin line by line. Each non-empty line becomes a Request.
    /// All CLI interactions share a single session for the lifetime of the process.
    async fn start(&self, tx: RequestSender) -> Result<()> {
        let session_id = Uuid::new_v4();
        let status = Arc::clone(&self.status);
        let eof = self.eof.clone();

        tokio::spawn(async move {
            let stdin = tokio::io::stdin();
            let mut reader = BufReader::new(stdin);
            let mut stdout = tokio::io::stdout();

            loop {
                // Render active work items above the prompt (cache updated by notify).
                // Build the panel string while holding the lock, then drop the guard
                // before .await so MutexGuard is not held across a suspension point.
                // A poisoned lock (a panic while held elsewhere) still holds a valid
                // Vec — recover it and keep rendering rather than crashing the REPL.
                let panel: Option<String> = {
                    let items_snapshot = match status.lock() {
                        Ok(items) => items,
                        Err(poisoned) => {
                            tracing::warn!("work item status lock poisoned — recovering");
                            poisoned.into_inner()
                        }
                    };
                    if items_snapshot.is_empty() {
                        None
                    } else {
                        Some(render_status_panel(&items_snapshot))
                    }
                };
                if let Some(panel) = panel {
                    stdout.write_all(panel.as_bytes()).await.ok();
                }

                stdout.write_all(b"\nYou: ").await.ok();
                stdout.flush().await.ok();

                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        eof.cancel(); // EOF (Ctrl+D) → request app shutdown
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("stdin read error: {e}");
                        eof.cancel();
                        break;
                    }
                }

                let message = line.trim().to_string();
                if message.is_empty() {
                    continue;
                }

                stdout.write_all(b"Haily: ").await.ok();
                stdout.flush().await.ok();

                if tx
                    .send(Request {
                        session_id,
                        adapter_id: "cli".to_string(),
                        message,
                        user_ref: None,
                    })
                    .await
                    .is_err()
                {
                    break; // orchestrator shut down
                }
            }
        });

        Ok(())
    }

    /// Streams response chunks to stdout. Text arrives as it's generated.
    async fn deliver(&self, _session_id: Uuid, chunk: ResponseChunk) -> Result<()> {
        let mut stdout = tokio::io::stdout();
        match chunk {
            ResponseChunk::Text(text) => {
                stdout.write_all(text.as_bytes()).await?;
                stdout.flush().await?;
            }
            ResponseChunk::Complete => {
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
            ResponseChunk::ToolApprovalRequest { tool, args, approval_id } => {
                let prompt = format!(
                    "\n[Tool approval needed]\nTool: {tool}\nArgs: {args}\nApprove? (y/n): "
                );
                stdout.write_all(prompt.as_bytes()).await?;
                stdout.flush().await?;
                let _ = approval_id;
            }
            ResponseChunk::ToolResult { name, ok } => {
                let status = if ok { "✓" } else { "✗" };
                let line = format!("[{status} {name}]\n");
                stdout.write_all(line.as_bytes()).await?;
            }
        }
        Ok(())
    }

    async fn notify(&self, msg: Notification) -> Result<()> {
        let mut stdout = tokio::io::stdout();
        let text = match msg {
            // Update the cached status; the REPL loop will render it before the next prompt.
            Notification::WorkItemsChanged(items) => {
                match self.status.lock() {
                    Ok(mut guard) => *guard = items,
                    Err(poisoned) => {
                        tracing::warn!("work item status lock poisoned — recovering");
                        *poisoned.into_inner() = items;
                    }
                }
                return Ok(());
            }
            Notification::MorningBrief(brief) => format!("\n[Morning Brief]\n{brief}\n"),
            Notification::Alert { title, body, urgent } => {
                let prefix = if urgent { "🔴" } else { "📢" };
                format!("\n{prefix} {title}\n{body}\n")
            }
            Notification::ReminderFired { title, .. } => {
                format!("\n⏰ Reminder: {title}\n")
            }
        };
        stdout.write_all(text.as_bytes()).await?;
        stdout.flush().await?;
        Ok(())
    }

    fn id(&self) -> &str {
        "cli"
    }
}

/// Compact status panel rendered above the `You:` prompt when tasks are active.
///
/// Each line: `  ⚙ "title" [last_tool] 30%`
/// A separator line follows the item list.
fn render_status_panel(items: &[WorkItemStatus]) -> String {
    let mut out = String::new();
    for item in items {
        let icon = match item.status.as_str() {
            "running"                => "⚙",
            "queued"                 => "⏳",
            "paused" | "interrupted" => "⏸",
            _                        => "•",
        };
        let title: String = if item.title.chars().count() > 42 {
            item.title.chars().take(41).collect::<String>() + "…"
        } else {
            item.title.clone()
        };
        let phase = item.phase.as_deref().map(|p| format!(" [{p}]")).unwrap_or_default();
        out.push_str(&format!("  {icon} \"{title}\"{phase} {}%\n", item.progress));
    }
    out.push_str("  ─────────────────────────────────\n");
    out
}
