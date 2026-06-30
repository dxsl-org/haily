use crate::{Adapter, Notification, Request, RequestSender, ResponseChunk};
use anyhow::Result;
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

pub struct CliAdapter;

impl CliAdapter {
    pub fn new() -> Self {
        Self
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

        tokio::spawn(async move {
            let stdin = tokio::io::stdin();
            let mut reader = BufReader::new(stdin);
            let mut stdout = tokio::io::stdout();

            loop {
                stdout.write_all(b"\nYou: ").await.ok();
                stdout.flush().await.ok();

                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("stdin read error: {e}");
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
                // Phase 07 orchestrator handles the approval response loop
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
