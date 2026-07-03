use crate::{
    Adapter, ApprovalResolver, Notification, Request, RequestSender, ResponseChunk, WorkItemStatus,
};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// A tool approval currently awaiting a y/n answer on stdin. Shared (same
/// allocation) between `deliver()` (which sets it when a `ToolApprovalRequest`
/// chunk arrives) and the stdin reader task (which checks it before treating a
/// line as chat input) — the CLI has exactly one input stream multiplexed between
/// two purposes, so this is the only way the reader can tell "the next line is a
/// y/n answer" from "the next line is a new chat message".
struct AwaitingApproval {
    approval_id: Uuid,
    session_id: Uuid,
}

pub struct CliAdapter {
    /// Active work items cached by notify(WorkItemsChanged). Read before each prompt.
    /// Only the REPL loop writes to stdout, so updating this from notify() is race-free.
    status: Arc<Mutex<Vec<WorkItemStatus>>>,
    /// Cancelled when the stdin reader hits EOF (Ctrl+D) or a fatal read error, so the
    /// app entry point can treat "input stream closed" as a shutdown request.
    eof: CancellationToken,
    /// `None` when no approval is pending. Set by `deliver()`, consumed (taken) by
    /// the reader task on its next line — see struct-level doc comment.
    awaiting: Arc<Mutex<Option<AwaitingApproval>>>,
    /// Injected by `haily-app::bootstrap` after the orchestrator exists (see
    /// `Adapter::set_approval_resolver`). `None` until then; a y/n line arriving
    /// before injection is impossible in practice (bootstrap wires this before
    /// `start()` runs) but is handled by falling back to "not awaiting" rather than
    /// panicking.
    resolver: Arc<Mutex<Option<Arc<dyn ApprovalResolver>>>>,
    /// `safety.disable_writes` kill switch (phase 3, C8), injected at bootstrap via
    /// `set_kill_switch` — the SAME `Arc<AtomicBool>` the orchestrator gates on. `None`
    /// until injected; the `/writes` command reports "not wired" rather than panicking.
    /// Lets the terminal user throw/clear the switch live with `/writes off` / `/writes on`.
    kill: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

impl CliAdapter {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(Vec::new())),
            eof: CancellationToken::new(),
            awaiting: Arc::new(Mutex::new(None)),
            resolver: Arc::new(Mutex::new(None)),
            kill: Arc::new(Mutex::new(None)),
        }
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
        let awaiting = Arc::clone(&self.awaiting);
        let resolver = Arc::clone(&self.resolver);
        let kill = Arc::clone(&self.kill);

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

                // `/writes on|off` throws or clears the kill switch (C8) live. Handled
                // before chat dispatch so it never reaches the orchestrator as a message.
                if let Some(arg) = message.strip_prefix("/writes") {
                    let arg = arg.trim();
                    let reply = handle_writes_command(&kill, arg);
                    let _ = stdout.write_all(reply.as_bytes()).await;
                    stdout.flush().await.ok();
                    continue;
                }

                // `/undo <journal_id>` is a shorthand for a precise chat instruction — there
                // is no dedicated undo backend command (phase 6 is surface-only); this just
                // rewrites to the same wording the GUI's Undo button sends, so the LLM's
                // existing `journal_undo` tool call and the approval gate handle the rest.
                if let Some(arg) = message.strip_prefix("/undo") {
                    match parse_undo_command(arg) {
                        Some(undo_message) => {
                            stdout.write_all(b"Haily: ").await.ok();
                            stdout.flush().await.ok();
                            if tx
                                .send(Request {
                                    session_id,
                                    adapter_id: "cli".to_string(),
                                    message: undo_message,
                                    user_ref: None,
                                })
                                .await
                                .is_err()
                            {
                                break; // orchestrator shut down
                            }
                        }
                        None => {
                            let _ = stdout
                                .write_all(b"[undo] usage: /undo <journal_id>\n")
                                .await;
                            stdout.flush().await.ok();
                        }
                    }
                    continue;
                }

                // If a tool approval is pending, the next non-empty line is the y/n
                // answer to it — NOT a new chat message. Consumed (taken) here so a
                // stray extra line afterward falls through to normal chat handling.
                let pending = match awaiting.lock() {
                    Ok(mut guard) => guard.take(),
                    Err(poisoned) => {
                        tracing::warn!("approval-awaiting lock poisoned — recovering");
                        poisoned.into_inner().take()
                    }
                };
                if let Some(pending) = pending {
                    let approved = matches!(message.to_ascii_lowercase().as_str(), "y" | "yes");
                    let resolved = match resolver.lock() {
                        Ok(guard) => guard
                            .as_ref()
                            .map(|r| r.resolve(pending.approval_id, pending.session_id, approved)),
                        Err(poisoned) => poisoned
                            .into_inner()
                            .as_ref()
                            .map(|r| r.resolve(pending.approval_id, pending.session_id, approved)),
                    };
                    match resolved {
                        Some(true) => {
                            let outcome = if approved { "approved" } else { "denied" };
                            let _ = stdout.write_all(format!("[{outcome}]\n").as_bytes()).await;
                        }
                        Some(false) => {
                            tracing::warn!("approval resolve() rejected (already resolved or session mismatch)");
                        }
                        None => {
                            tracing::warn!(
                                "y/n received but no approval resolver is wired yet — ignoring"
                            );
                        }
                    }
                    stdout.flush().await.ok();
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
    async fn deliver(&self, session_id: Uuid, chunk: ResponseChunk) -> Result<()> {
        let mut stdout = tokio::io::stdout();
        match chunk {
            ResponseChunk::Text(text) => {
                stdout.write_all(text.as_bytes()).await?;
                stdout.flush().await?;
            }
            ResponseChunk::Error(text) => {
                // CLI doesn't buffer (each Text chunk is written immediately), so
                // there's no fused-message risk here — just render it distinctly.
                let line = format!("\n⚠️ {text}\n");
                stdout.write_all(line.as_bytes()).await?;
                stdout.flush().await?;
            }
            ResponseChunk::Complete => {
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
            ResponseChunk::ToolApprovalRequest {
                tool,
                args,
                approval_id,
                origin,
            } => {
                // Set BEFORE printing the prompt: the reader task could otherwise
                // observe the prompt (via stdout ordering) before `awaiting` is set,
                // but since both this write and the reader's next read_line happen
                // after this store, ordering here — not after — is what makes the
                // next line unambiguously route as a y/n answer.
                match self.awaiting.lock() {
                    Ok(mut guard) => {
                        *guard = Some(AwaitingApproval {
                            approval_id,
                            session_id,
                        })
                    }
                    Err(poisoned) => {
                        *poisoned.into_inner() = Some(AwaitingApproval {
                            approval_id,
                            session_id,
                        })
                    }
                }
                // `origin` (e.g. "L1:developer") is display-only — who is asking.
                let who = origin
                    .as_deref()
                    .map(|o| format!(" [{o}]"))
                    .unwrap_or_default();
                let prompt = format!(
                    "\n[Tool approval needed]{who}\nTool: {tool}\nArgs: {args}\nApprove? (y/n): "
                );
                stdout.write_all(prompt.as_bytes()).await?;
                stdout.flush().await?;
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
            Notification::Alert {
                title,
                body,
                urgent,
            } => {
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

    fn set_approval_resolver(&self, resolver: Arc<dyn ApprovalResolver>) {
        match self.resolver.lock() {
            Ok(mut guard) => *guard = Some(resolver),
            Err(poisoned) => *poisoned.into_inner() = Some(resolver),
        }
    }

    fn set_kill_switch(&self, kill: Arc<AtomicBool>) {
        match self.kill.lock() {
            Ok(mut guard) => *guard = Some(kill),
            Err(poisoned) => *poisoned.into_inner() = Some(kill),
        }
    }

    fn id(&self) -> &str {
        "cli"
    }
}

/// Parse and apply a `/writes` REPL command. `arg` is the text after `/writes`.
/// Returns the line to echo to the user. Pure aside from the atomic store so it is unit-
/// testable. `on` = writes enabled (switch OFF); `off` = writes disabled (switch ON).
fn handle_writes_command(kill: &Arc<Mutex<Option<Arc<AtomicBool>>>>, arg: &str) -> String {
    let handle = match kill.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let Some(handle) = handle else {
        return "[writes] kill switch not wired yet\n".to_string();
    };
    match arg {
        "off" => {
            handle.store(true, Ordering::Release);
            "[writes] DISABLED — new writes are blocked (in-flight writes are not stopped)\n"
                .to_string()
        }
        "on" => {
            handle.store(false, Ordering::Release);
            "[writes] ENABLED — new writes allowed\n".to_string()
        }
        "" | "status" => {
            let disabled = handle.load(Ordering::Acquire);
            format!(
                "[writes] currently {}\n",
                if disabled { "DISABLED" } else { "ENABLED" }
            )
        }
        _ => "[writes] usage: /writes on | off | status\n".to_string(),
    }
}

/// Parse a `/undo` REPL command. `arg` is the text after `/undo`. Returns the chat
/// message to forward to the orchestrator, or `None` (print usage, send nothing) if no
/// id was given — pure and unit-testable, mirroring `handle_writes_command`.
fn parse_undo_command(arg: &str) -> Option<String> {
    let id = arg.trim();
    if id.is_empty() {
        return None;
    }
    Some(format!("Undo the action with journal id \"{id}\"."))
}

/// Compact status panel rendered above the `You:` prompt when tasks are active.
///
/// Each line: `  ⚙ "title" [last_tool] 30%`
/// A separator line follows the item list.
fn render_status_panel(items: &[WorkItemStatus]) -> String {
    let mut out = String::new();
    for item in items {
        let icon = match item.status.as_str() {
            "running" => "⚙",
            "queued" => "⏳",
            "paused" | "interrupted" => "⏸",
            _ => "•",
        };
        let title: String = if item.title.chars().count() > 42 {
            item.title.chars().take(41).collect::<String>() + "…"
        } else {
            item.title.clone()
        };
        let phase = item
            .phase
            .as_deref()
            .map(|p| format!(" [{p}]"))
            .unwrap_or_default();
        out.push_str(&format!("  {icon} \"{title}\"{phase} {}%\n", item.progress));
    }
    out.push_str("  ─────────────────────────────────\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Records the last call so tests can assert what the CLI actually asked the
    /// resolver to do, without needing a real `ApprovalBroker`.
    struct RecordingResolver {
        called: AtomicBool,
        last_approved: Mutex<Option<bool>>,
    }

    impl ApprovalResolver for RecordingResolver {
        fn resolve(&self, _approval_id: Uuid, _session_id: Uuid, approved: bool) -> bool {
            self.called.store(true, Ordering::SeqCst);
            *self.last_approved.lock().unwrap() = Some(approved);
            true
        }
    }

    /// `deliver()` on a `ToolApprovalRequest` must set the shared awaiting-state —
    /// this is the mechanism the reader task depends on to route the next line as a
    /// y/n answer instead of a chat message.
    #[tokio::test]
    async fn deliver_tool_approval_request_sets_awaiting_state() {
        let cli = CliAdapter::new();
        let approval_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();

        cli.deliver(
            session_id,
            ResponseChunk::ToolApprovalRequest {
                tool: "worktree_apply".to_string(),
                args: "{}".to_string(),
                approval_id,
                origin: None,
            },
        )
        .await
        .expect("deliver should not error");

        let pending = cli.awaiting.lock().unwrap();
        let pending = pending
            .as_ref()
            .expect("awaiting state must be set after a ToolApprovalRequest chunk");
        assert_eq!(pending.approval_id, approval_id);
        assert_eq!(pending.session_id, session_id);
    }

    /// Simulates the reader task's routing decision directly: once a resolver is
    /// injected and an approval is pending, a "y" answer must call `resolve(...,
    /// approved = true)` — proving the y/n path reaches the resolver rather than
    /// being sent as a chat `Request`.
    #[tokio::test]
    async fn pending_approval_routes_yes_to_resolver_as_approved() {
        let cli = CliAdapter::new();
        let resolver = Arc::new(RecordingResolver {
            called: AtomicBool::new(false),
            last_approved: Mutex::new(None),
        });
        cli.set_approval_resolver(resolver.clone());

        let approval_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        cli.deliver(
            session_id,
            ResponseChunk::ToolApprovalRequest {
                tool: "task_delete".to_string(),
                args: "{}".to_string(),
                approval_id,
                origin: None,
            },
        )
        .await
        .unwrap();

        // Mirror the reader task's own routing logic against the shared state.
        let pending = cli
            .awaiting
            .lock()
            .unwrap()
            .take()
            .expect("must still be pending");
        let approved = matches!("y", "y" | "yes");
        resolver.resolve(pending.approval_id, pending.session_id, approved);

        assert!(
            resolver.called.load(Ordering::SeqCst),
            "resolver must be invoked for a y/n answer, not skipped"
        );
        assert_eq!(*resolver.last_approved.lock().unwrap(), Some(true));
        assert!(
            cli.awaiting.lock().unwrap().is_none(),
            "awaiting state must be cleared (taken) after being consumed"
        );
    }

    /// A well-formed `/undo <id>` builds a precise chat instruction naming that id — the
    /// same wording the GUI's Undo button sends, so both surfaces drive the LLM's
    /// `journal_undo` tool call identically.
    #[test]
    fn parse_undo_command_builds_message_for_given_id() {
        let msg = parse_undo_command(" abc-123 ").expect("id present");
        assert_eq!(msg, "Undo the action with journal id \"abc-123\".");
    }

    /// An unknown/garbage id is still just forwarded as text — the tool layer (not the
    /// CLI) is responsible for a clean "not found" reply, so this must never panic.
    #[test]
    fn parse_undo_command_forwards_unknown_id_without_panicking() {
        let msg = parse_undo_command("does-not-exist").expect("id present");
        assert!(msg.contains("does-not-exist"));
    }

    /// Missing id → `None` (print usage), not a malformed "Undo the action with journal
    /// id \"\"." request sent to the orchestrator.
    #[test]
    fn parse_undo_command_missing_id_returns_none() {
        assert_eq!(parse_undo_command(""), None);
        assert_eq!(parse_undo_command("   "), None);
    }
}
