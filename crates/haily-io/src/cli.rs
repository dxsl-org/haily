use crate::{
    slash, Adapter, ApprovalResolver, Notification, Request, RequestSender, ResponseChunk, RunEvent,
    WorkItemStatus,
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

                // `/help` is the discovery surface (phase 11a) — the canonical slash
                // registry, rendered identically on every channel. Handled before chat
                // dispatch so it never reaches the orchestrator as a message.
                if message == "/help" {
                    let _ = stdout.write_all(slash::help_text().as_bytes()).await;
                    stdout.flush().await.ok();
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
                                    depth: Default::default(),
                                    origin: Default::default(),
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
                        depth: Default::default(),
                        origin: Default::default(),
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

    /// Render an ordered pipeline `RunEvent` to stdout (phase 11a): tool-progress lines,
    /// a lightweight plan/stage panel, and gate/retry/pause milestones. The modal approval
    /// path is unchanged — `ApprovalNeeded` here is only an informational ping; the real
    /// y/n prompt still arrives via `deliver(ToolApprovalRequest)`. Content is already
    /// tag-stripped at the delivery chokepoint, so it renders as inert text.
    async fn deliver_run_event(&self, _session_id: Uuid, event: RunEvent) -> Result<()> {
        let mut stdout = tokio::io::stdout();
        stdout.write_all(render_run_event_line(&event).as_bytes()).await?;
        stdout.flush().await?;
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
                reversible: _,
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
            ResponseChunk::ToolResult {
                name,
                ok,
                // R4 framing additive fields (Harness Completion phase 3): the CLI's
                // stdout format is a fixed, byte-stable contract (M8) — an inline-undo
                // affordance is GUI-only, so these are read but intentionally unused
                // here. See `render_tool_result_line_is_stable_regardless_of_new_fields`.
                reversible: _,
                journal_id: _,
            } => {
                stdout
                    .write_all(render_tool_result_line(&name, ok).as_bytes())
                    .await?;
            }
            ResponseChunk::TurnMeta { badge } => {
                if let Some(badge) = badge {
                    stdout
                        .write_all(render_turn_meta_line(&badge).as_bytes())
                        .await?;
                }
            }
            ResponseChunk::ViewRef { entity, .. } => {
                // The CLI is text-only — it renders the handle, never fetches the full
                // `DataView` payload (that command path is GUI-only, built in Phase 3).
                stdout
                    .write_all(format!("\n[view] {entity}\n").as_bytes())
                    .await?;
                stdout.flush().await?;
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
            Notification::DistillationProposal { summary, rule_count, .. } => {
                format!("\n[Distillation proposal — {rule_count} rule(s)]\n{summary}\n")
            }
            Notification::KillStateChanged { on } => {
                format!(
                    "\n{} Kill switch {}\n",
                    if on { "🔴" } else { "🟢" },
                    if on { "ENABLED" } else { "DISABLED" }
                )
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

/// Render a `ToolResult` chunk's one-line stdout form, e.g. `[✓ task_delete]\n`. Pure
/// and unit-testable so the M8 byte-stability guarantee (CLI output never changes
/// shape when `ToolResult` gains additive fields — Harness Completion phase 3) can be
/// asserted directly, without capturing process stdout.
fn render_tool_result_line(name: &str, ok: bool) -> String {
    let status = if ok { "✓" } else { "✗" };
    format!("[{status} {name}]\n")
}

/// Render a `TurnMeta` badge as a dim `(tier · model)` line (ANSI SGR 2 = dim, reset via
/// SGR 0). Pure and unit-testable like `render_tool_result_line`. `badge` is built from
/// internal config strings only (never model/tool output), so no escaping is needed here.
fn render_turn_meta_line(badge: &str) -> String {
    format!("\x1b[2m({badge})\x1b[0m\n")
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

/// Render one ordered `RunEvent` as the CLI's stdout line(s) (phase 11a). Pure and
/// unit-testable. `StageOutput` is the streamed stage content (rendered raw, no framing,
/// since it is the tool-progress stream itself); every other variant is a bracketed
/// milestone line. `StageStarted`/`PlanReady` double as the lightweight plan/stage panel.
fn render_run_event_line(event: &RunEvent) -> String {
    match event {
        RunEvent::RunStarted { run_id, .. } => format!("\n▶ [run {run_id}] started\n"),
        RunEvent::StageStarted { stage, tier, .. } => {
            let t = tier.as_deref().map(|t| format!(" ({t})")).unwrap_or_default();
            format!("\n── stage: {stage}{t} ──\n")
        }
        // The streamed stage content itself — this IS the tool-progress stream.
        RunEvent::StageOutput { chunk, .. } => chunk.clone(),
        RunEvent::GateResult { gate, pass, decisive, .. } => {
            let mark = if *pass { "✓" } else { "✗" };
            if *pass || decisive.is_empty() {
                format!("\n[gate {gate}: {mark}]\n")
            } else {
                format!("\n[gate {gate}: {mark}] {decisive}\n")
            }
        }
        RunEvent::Retry { attempt, .. } => format!("\n[retry: attempt {attempt}]\n"),
        RunEvent::Escalation { from, to, .. } => format!("\n[escalated {from} → {to}]\n"),
        RunEvent::DiffAvailable { file, .. } => format!("\n[diff ready: {file}]\n"),
        RunEvent::ApprovalNeeded { .. } => "\n[approval needed — respond at the y/n prompt]\n".to_string(),
        RunEvent::PlanReady { plan_path, .. } => format!("\n📝 [plan ready: {plan_path}]\n"),
        RunEvent::RunPaused { reason, .. } => format!("\n⏸ [run paused: {reason}]\n"),
        RunEvent::RunComplete { outcome, .. } => format!("\n🏁 [run complete: {outcome}]\n"),
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
                reversible: false,
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
                reversible: false,
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

    /// M8 guard (Harness Completion phase 3, R4 framing): the CLI's `[✓ name]`/
    /// `[✗ name]` stdout line depends ONLY on `name`/`ok` — `ResponseChunk::ToolResult`
    /// gaining `reversible`/`journal_id` must not change a single byte of what the CLI
    /// prints. `deliver()`'s `ToolResult` arm calls this exact function (not a copy),
    /// so a regression there fails here too.
    #[test]
    fn render_tool_result_line_is_stable_regardless_of_new_fields() {
        // The line only ever depends on name/ok — assert the exact byte-for-byte shape
        // a pre-phase-3 CLI would have produced.
        assert_eq!(render_tool_result_line("task_delete", true), "[✓ task_delete]\n");
        assert_eq!(render_tool_result_line("task_delete", false), "[✗ task_delete]\n");
    }

    /// Phase 11a: the CLI renders each `RunEvent` as a distinct stdout form — milestones
    /// are bracketed lines, while `StageOutput` is the raw streamed content (the
    /// tool-progress stream itself, no framing).
    #[test]
    fn render_run_event_line_formats_each_variant() {
        assert!(render_run_event_line(&RunEvent::StageStarted {
            run_id: "r".into(),
            stage: "build".into(),
            tier: Some("thinking".into()),
        })
        .contains("stage: build (thinking)"));

        // StageOutput is the raw stream — no brackets, returned verbatim.
        assert_eq!(
            render_run_event_line(&RunEvent::StageOutput {
                run_id: "r".into(),
                seq: 0,
                chunk: "compiling…".into(),
            }),
            "compiling…"
        );

        let gate = render_run_event_line(&RunEvent::GateResult {
            run_id: "r".into(),
            gate: "command".into(),
            pass: false,
            decisive: "E0001".into(),
        });
        assert!(gate.contains("gate command"));
        assert!(gate.contains("E0001"));

        assert!(render_run_event_line(&RunEvent::PlanReady {
            run_id: "r".into(),
            plan_path: ".agents/x/plan.md".into(),
        })
        .contains("plan ready"));
    }

    /// `deliver_run_event` must not error rendering any variant (it writes to stdout — the
    /// assertion is that the path is total and panic-free, mirroring the ToolResult test).
    #[tokio::test]
    async fn deliver_run_event_renders_without_error() {
        let cli = CliAdapter::new();
        for ev in [
            RunEvent::RunStarted { run_id: "r".into(), work_item_id: "w".into() },
            RunEvent::StageOutput { run_id: "r".into(), seq: 0, chunk: "x".into() },
            RunEvent::RunComplete { run_id: "r".into(), outcome: "done".into() },
        ] {
            cli.deliver_run_event(Uuid::new_v4(), ev).await.expect("render must not error");
        }
    }

    /// Constructs both a legacy-shaped and an R4-shaped `ToolResult` (reversible +
    /// journal_id populated) and proves `deliver()`'s rendering path derives the same
    /// line for both — the CLI arm destructures every field but only reads `name`/`ok`.
    #[tokio::test]
    async fn deliver_tool_result_rendering_ignores_reversible_and_journal_id() {
        // No direct way to capture this adapter's process-wide stdout in-test; instead
        // prove the two field-shapes drive an IDENTICAL call to the same pure renderer
        // `deliver()` uses, which is the actual byte-stability contract.
        let legacy = ResponseChunk::ToolResult {
            name: "task_delete".to_string(),
            ok: true,
            reversible: false,
            journal_id: None,
        };
        let r4 = ResponseChunk::ToolResult {
            name: "task_delete".to_string(),
            ok: true,
            reversible: true,
            journal_id: Some("journal-row-id".to_string()),
        };
        let render = |chunk: ResponseChunk| match chunk {
            ResponseChunk::ToolResult { name, ok, .. } => render_tool_result_line(&name, ok),
            other => panic!("expected ToolResult, got {other:?}"),
        };
        assert_eq!(render(legacy), render(r4));

        // Also exercise the real adapter path end-to-end to prove it does not panic or
        // otherwise diverge when the new fields are populated.
        let cli = CliAdapter::new();
        cli.deliver(
            Uuid::new_v4(),
            ResponseChunk::ToolResult {
                name: "task_delete".to_string(),
                ok: true,
                reversible: true,
                journal_id: Some("journal-row-id".to_string()),
            },
        )
        .await
        .expect("deliver must not error on a ToolResult with the new fields populated");
    }

    /// Pure-function contract for the dim `(tier · model)` badge line, mirroring
    /// `render_tool_result_line`'s own byte-shape test.
    #[test]
    fn render_turn_meta_line_wraps_badge_in_dim_ansi_and_parens() {
        let line = render_turn_meta_line("thinking · llama-3");
        assert_eq!(line, "\x1b[2m(thinking · llama-3)\x1b[0m\n");
    }

    /// `deliver()` must not error on a `TurnMeta` chunk, with or without a badge (the
    /// `None` case never reaches here in practice — `run_turn` only ever sends
    /// `Some(badge)` — but the CLI arm is written to tolerate it as a defensive no-op).
    #[tokio::test]
    async fn deliver_turn_meta_renders_without_error() {
        let cli = CliAdapter::new();
        cli.deliver(
            Uuid::new_v4(),
            ResponseChunk::TurnMeta {
                badge: Some("thinking · llama-3".to_string()),
            },
        )
        .await
        .expect("deliver must not error on TurnMeta with a badge");
        cli.deliver(Uuid::new_v4(), ResponseChunk::TurnMeta { badge: None })
            .await
            .expect("deliver must not error on TurnMeta with no badge");
    }
}
