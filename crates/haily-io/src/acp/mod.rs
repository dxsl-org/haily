//! ACP (Agent Client Protocol) coding channel — a 4th `Adapter` (Sub-Agent + Skill
//! Architecture phase 12).
//!
//! An ACP-capable editor (Zed and friends) becomes a code-viewing/reviewing front-end for
//! Haily's EXISTING coding pipeline: Haily is the agent behind the editor; edits stream in as
//! native inline diffs; approvals and the plan checkpoint arrive as native permission prompts.
//! This is a **rendering + gating surface only** — no engine logic lives here. It reuses the
//! existing orchestrator (`start`/`deliver` mpsc contract), the `ApprovalGate`/`ApprovalResolver`
//! seam, and the ordered `RunEvent` stream.
//!
//! ## Transport
//! Newline-delimited JSON-RPC 2.0 over stdio (the plan's hand-rolled fallback dialect — see the
//! phase Deviation Log for why the `agent-client-protocol` crate was not used). stdout carries
//! ONLY protocol frames; ALL logs go to stderr (the `run_acp` entry point configures tracing).
//! The transport is bidirectional and task-per-request: the read loop spawns a task per inbound
//! request so a long-running turn never blocks the loop from routing a `request_permission`
//! response back to the awaiting `deliver()`.
//!
//! ## Security
//! `session_id` is the sole auth boundary. Every editor-supplied prompt is tag-stripped before
//! it reaches the model. ACP is a **Chat-class** transport (never `Cli`), so it can never reach
//! eval mode's privileged plan-gate bypass. A sensitive-path write is prompted UNCONDITIONALLY
//! even in the most permissive session mode.

pub mod jsonrpc;
pub mod protocol;
pub mod session;

use crate::{Adapter, ApprovalResolver, Notification, RequestSender, ResponseChunk, RunEvent};
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use haily_types::SessionTranscript;
use jsonrpc::Incoming;
use protocol::{PermissionDecision, SessionMode};
use serde_json::{json, Value};
use session::AcpSessions;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// The adapter id — matches `Request::adapter_id` and the `AdapterManager` routing key.
pub const ADAPTER_ID: &str = "acp";

/// Max wait for an editor to answer a `session/request_permission`. On timeout the approval
/// is DENIED (fail-safe) — a slow or dead editor can never leave a destructive action pending.
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(60);

/// Max diff preview text pulled from disk for an edit block (a huge file cannot flood a frame).
const MAX_EDIT_PREVIEW: usize = 256 * 1024;

/// Boxed async frame writer — stdout in production, an in-memory duplex in tests. Boxed (not a
/// generic) so the concrete writer type does not leak into the `Adapter` impl.
type FrameWriter = tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>;

/// The post-construction-injected transcript provider slot (same wiring shape as the approval
/// resolver). Aliased so the several handlers that thread it stay readable.
pub type TranscriptSlot = Arc<Mutex<Option<Arc<dyn SessionTranscript>>>>;

/// The JSON-RPC transport: the single guarded writer, the map of outbound requests awaiting a
/// response, and the per-session turn-completion signals `deliver(Complete)` fires.
pub struct AcpConnection {
    writer: FrameWriter,
    /// Outbound request id → responder. The read loop routes an inbound `Response` here so the
    /// `deliver()` task awaiting a `request_permission` answer wakes.
    pending: DashMap<String, oneshot::Sender<Value>>,
    /// Per-session (internal id) turn-complete signal. Registered by the prompt handler, fired
    /// by `deliver(Complete)`, so the handler can send the `session/prompt` response only once
    /// the whole turn has finished.
    completions: DashMap<Uuid, oneshot::Sender<()>>,
    next_id: AtomicU64,
}

impl AcpConnection {
    pub fn new(writer: Box<dyn AsyncWrite + Send + Unpin>) -> Self {
        Self {
            writer: tokio::sync::Mutex::new(writer),
            pending: DashMap::new(),
            completions: DashMap::new(),
            next_id: AtomicU64::new(1),
        }
    }

    /// Write one pre-built frame line. All stdout output funnels through here, so the
    /// stdout-frame-discipline invariant reduces to "every caller passes a `jsonrpc.rs` frame".
    async fn write_line(&self, line: String) -> Result<()> {
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes()).await?;
        w.flush().await?;
        Ok(())
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_line(jsonrpc::notification_frame(method, params)).await
    }

    pub async fn respond_ok(&self, id: &Value, result: Value) -> Result<()> {
        self.write_line(jsonrpc::response_ok_frame(id, result)).await
    }

    pub async fn respond_err(&self, id: &Value, code: i64, message: &str) -> Result<()> {
        self.write_line(jsonrpc::response_err_frame(id, code, message)).await
    }

    /// Issue a request to the client and await its response, bounded by `timeout` AND by
    /// `cancel` (a `session/cancel` fires it). Returns `None` on either timeout or cancel — the
    /// caller treats both as a fail-safe deny. The oneshot is registered BEFORE the frame is
    /// written so a fast response cannot be missed, and the pending entry is always cleaned up.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id_val = json!(id);
        let key = id.to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.insert(key.clone(), tx);
        if let Err(e) = self.write_line(jsonrpc::request_frame(&id_val, method, params)).await {
            tracing::warn!("acp: failed to write request {method}: {e:#}");
            self.pending.remove(&key);
            return None;
        }
        let result = tokio::select! {
            _ = cancel.cancelled() => None,
            r = tokio::time::timeout(timeout, rx) => match r {
                Ok(Ok(v)) => Some(v),
                _ => None, // timeout or sender dropped → deny
            },
        };
        // Always drop the pending entry (a late/never response has nowhere to go now).
        self.pending.remove(&key);
        result
    }

    /// Route an inbound `Response` frame to the awaiting `request()` caller. A response with no
    /// matching pending id (already timed out, or unsolicited) is logged and dropped.
    pub fn resolve_response(&self, id: &Value, value: Value) {
        let key = id_to_key(id);
        if let Some((_, tx)) = self.pending.remove(&key) {
            let _ = tx.send(value);
        } else {
            tracing::warn!("acp: response for unknown/expired request id {key}");
        }
    }

    fn register_completion(&self, haily_id: Uuid) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.completions.insert(haily_id, tx);
        rx
    }

    fn signal_complete(&self, haily_id: &Uuid) {
        if let Some((_, tx)) = self.completions.remove(haily_id) {
            let _ = tx.send(());
        }
    }
}

fn id_to_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The ACP adapter. Cheap to clone-share via the internal `Arc`s.
pub struct AcpAdapter {
    conn: Arc<AcpConnection>,
    sessions: Arc<AcpSessions>,
    resolver: Arc<Mutex<Option<Arc<dyn ApprovalResolver>>>>,
    kill: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    transcript: TranscriptSlot,
    /// Fires when stdin closes (the editor disconnected) or a fatal read error occurs, so the
    /// entry point can treat "the ACP peer went away" as a shutdown request — mirrors
    /// `CliAdapter::eof_token`.
    eof: CancellationToken,
}

impl AcpAdapter {
    /// Build over real stdio (production). Logs MUST already be routed to stderr by the caller.
    pub fn new() -> Self {
        Self::with_writer(Box::new(tokio::io::stdout()))
    }

    /// Build over an arbitrary writer — the seam tests use to inject an in-memory duplex and
    /// assert the exact frames emitted to "stdout".
    pub fn with_writer(writer: Box<dyn AsyncWrite + Send + Unpin>) -> Self {
        Self {
            conn: Arc::new(AcpConnection::new(writer)),
            sessions: Arc::new(AcpSessions::new()),
            resolver: Arc::new(Mutex::new(None)),
            kill: Arc::new(Mutex::new(None)),
            transcript: Arc::new(Mutex::new(None)),
            eof: CancellationToken::new(),
        }
    }

    /// A token that fires when the ACP peer's stdin closes. The entry point races it alongside
    /// OS signals so an editor disconnect quits cleanly instead of leaving an idle process.
    pub fn eof_token(&self) -> CancellationToken {
        self.eof.clone()
    }

    fn resolver_handle(&self) -> Option<Arc<dyn ApprovalResolver>> {
        self.resolver.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Default for AcpAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for AcpAdapter {
    /// Spawn the stdio read loop. Each inbound REQUEST is dispatched on its own task so a
    /// long turn never blocks the loop from routing a `request_permission` response back to a
    /// waiting `deliver()`. Notifications and responses are handled inline (they never block).
    async fn start(&self, tx: RequestSender) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let sessions = Arc::clone(&self.sessions);
        let transcript = Arc::clone(&self.transcript);
        let eof = self.eof.clone();
        let tx = Arc::new(tx);

        tokio::spawn(async move {
            let mut reader = BufReader::new(tokio::io::stdin());
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        tracing::info!("acp: stdin closed — read loop exiting");
                        eof.cancel();
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("acp: stdin read error: {e}");
                        eof.cancel();
                        break;
                    }
                }
                match jsonrpc::parse_incoming(&line) {
                    Incoming::Request { id, method, params } => {
                        let conn = Arc::clone(&conn);
                        let sessions = Arc::clone(&sessions);
                        let transcript = Arc::clone(&transcript);
                        let tx = Arc::clone(&tx);
                        tokio::spawn(async move {
                            handle_request(&conn, &sessions, &transcript, &tx, id, &method, params).await;
                        });
                    }
                    Incoming::Notification { method, params } => {
                        handle_notification(&conn, &sessions, &method, &params);
                    }
                    Incoming::Response { id, result, error } => {
                        conn.resolve_response(&id, result.unwrap_or_else(|| error.unwrap_or(Value::Null)));
                    }
                    Incoming::Invalid(reason) => {
                        tracing::warn!("acp: dropping malformed inbound line: {reason}");
                    }
                }
            }
        });
        Ok(())
    }

    /// Stream one orchestrator `ResponseChunk` to the editor as a `session/update`, EXCEPT a
    /// `ToolApprovalRequest`, which becomes a `session/request_permission` bridged onto the
    /// existing `ApprovalGate` (via the injected `ApprovalResolver`).
    async fn deliver(&self, haily_id: Uuid, chunk: ResponseChunk) -> Result<()> {
        let Some(acp_id) = self.sessions.acp_id(&haily_id) else {
            // Not an ACP-owned session — nothing to render.
            return Ok(());
        };
        match chunk {
            ResponseChunk::Text(text) => {
                self.conn.notify(protocol::M_SESSION_UPDATE, protocol::text_update(&acp_id, "assistant", &text)).await?;
            }
            ResponseChunk::Error(text) => {
                self.conn.notify(protocol::M_SESSION_UPDATE, protocol::text_update(&acp_id, "assistant", &format!("⚠️ {text}"))).await?;
            }
            ResponseChunk::Complete => {
                // Wake the prompt handler so it can send the session/prompt response.
                self.conn.signal_complete(&haily_id);
            }
            ResponseChunk::ToolResult { name, ok, .. } => {
                let mark = if ok { "✓" } else { "✗" };
                self.conn
                    .notify(protocol::M_SESSION_UPDATE, protocol::text_update(&acp_id, "assistant", &format!("[{mark} {name}]")))
                    .await?;
            }
            ResponseChunk::ToolApprovalRequest { tool, args, approval_id, reversible, .. } => {
                self.surface_approval(&acp_id, haily_id, &tool, &args, approval_id, reversible).await?;
            }
        }
        Ok(())
    }

    /// Map an ordered `RunEvent` to a `session/update`. Content is already tag-stripped at the
    /// delivery chokepoint, so it renders as inert data.
    async fn deliver_run_event(&self, haily_id: Uuid, event: RunEvent) -> Result<()> {
        if let Some(acp_id) = self.sessions.acp_id(&haily_id) {
            self.conn.notify(protocol::M_SESSION_UPDATE, protocol::run_event_update(&acp_id, &event)).await?;
        }
        Ok(())
    }

    /// Proactive notifications broadcast to every live ACP session as a plain agent message.
    /// `WorkItemsChanged` has no ACP surface (no persistent panel) — skipped, like Telegram.
    async fn notify(&self, msg: Notification) -> Result<()> {
        if matches!(msg, Notification::WorkItemsChanged(_)) {
            return Ok(());
        }
        let text = match &msg {
            Notification::MorningBrief(b) => format!("🌅 {b}"),
            Notification::Alert { title, body, .. } => format!("{title}\n{body}"),
            Notification::ReminderFired { title, .. } => format!("⏰ {title}"),
            Notification::DistillationProposal { summary, rule_count, .. } => {
                format!("🧪 distillation proposal ({rule_count} rule(s))\n{summary}")
            }
            Notification::WorkItemsChanged(_) => return Ok(()),
            Notification::KillStateChanged { on } => {
                format!(
                    "{} kill switch changed — writes {}",
                    if *on { "🔴" } else { "🟢" },
                    if *on { "disabled" } else { "enabled" }
                )
            }
        };
        for acp_id in self.sessions.list() {
            self.conn
                .notify(protocol::M_SESSION_UPDATE, protocol::text_update(&acp_id, "assistant", &text))
                .await
                .ok();
        }
        Ok(())
    }

    fn set_approval_resolver(&self, resolver: Arc<dyn ApprovalResolver>) {
        *self.resolver.lock().unwrap_or_else(|e| e.into_inner()) = Some(resolver);
    }

    fn set_kill_switch(&self, kill: Arc<AtomicBool>) {
        *self.kill.lock().unwrap_or_else(|e| e.into_inner()) = Some(kill);
    }

    fn set_session_transcript(&self, t: Arc<dyn SessionTranscript>) {
        *self.transcript.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
    }

    fn id(&self) -> &str {
        ADAPTER_ID
    }
}

impl AcpAdapter {
    /// The permission bridge. Applies the session-mode policy (with the UNCONDITIONAL
    /// sensitive-path prompt), and for a prompt-required approval issues an ACP
    /// `request_permission` (with an edit-diff preview when the tool targets a file), waits
    /// ≤60s, then resolves the pending `ApprovalGate` via the injected `ApprovalResolver`.
    /// Timeout / reject / cancel ⇒ deny.
    async fn surface_approval(
        &self,
        acp_id: &str,
        haily_id: Uuid,
        tool: &str,
        args: &str,
        approval_id: Uuid,
        reversible: bool,
    ) -> Result<()> {
        let rel_path = protocol::extract_write_path(args);
        let sensitive = rel_path.as_deref().map(protocol::is_sensitive_path).unwrap_or(false);
        let mode = self.sessions.mode(acp_id);

        // AUTO-ALLOW path: no editor prompt, resolve immediately (session-mode policy).
        if protocol::decide(mode, reversible, sensitive) == PermissionDecision::AutoAllow {
            self.resolve(approval_id, haily_id, true);
            return Ok(());
        }

        // PROMPT path: render an edit-diff preview when we can resolve the file, ask the editor.
        let edit_diff = rel_path.as_deref().and_then(|rel| {
            let abs = resolve_abs(self.sessions.cwd_for_haily(&haily_id).as_deref(), rel)?;
            Some(protocol::compute_edit_diff(&abs, &read_preview(&abs)))
        });
        let cancel = CancellationToken::new();
        self.sessions.add_pending(&haily_id, approval_id, cancel.clone());
        let params = protocol::permission_request_params(acp_id, tool, args, edit_diff);
        let outcome = self.conn.request(protocol::M_REQUEST_PERMISSION, params, PERMISSION_TIMEOUT, &cancel).await;
        self.sessions.remove_pending(&haily_id, &approval_id);

        let (approved, mode_update) = match outcome {
            Some(result) => protocol::interpret_permission_outcome(&result),
            None => (false, None), // timeout / write failure ⇒ deny
        };
        if let Some(new_mode) = mode_update {
            self.sessions.set_mode(acp_id, new_mode);
        }
        self.resolve(approval_id, haily_id, approved);
        Ok(())
    }

    /// Resolve a pending approval through the SAME `ApprovalResolver` seam every channel uses.
    /// A `false` result (already resolved / session mismatch) or an unwired resolver is logged.
    fn resolve(&self, approval_id: Uuid, haily_id: Uuid, approved: bool) {
        match self.resolver_handle() {
            Some(r) => {
                if !r.resolve(approval_id, haily_id, approved) {
                    tracing::warn!(%approval_id, "acp: approval resolve rejected (already resolved or session mismatch)");
                }
            }
            None => tracing::warn!("acp: approval decided but no resolver is wired yet — ignoring"),
        }
    }
}

/// Resolve `rel` (a workspace-relative or absolute write path from tool args) to an absolute
/// path for the edit-diff preview, using the session `cwd` when the path is relative. `None`
/// if no base is available for a relative path — the preview is then simply omitted.
fn resolve_abs(cwd: Option<&str>, rel: &str) -> Option<PathBuf> {
    let p = PathBuf::from(rel);
    if p.is_absolute() {
        return Some(p);
    }
    cwd.map(|c| PathBuf::from(c).join(rel))
}

fn read_preview(abs: &std::path::Path) -> String {
    let text = std::fs::read_to_string(abs).unwrap_or_default();
    if text.len() <= MAX_EDIT_PREVIEW {
        text
    } else {
        // The preview only needs the current on-disk text; a huge file is truncated on a char
        // boundary so the diff block stays a single well-formed frame.
        let mut end = MAX_EDIT_PREVIEW;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text[..end].to_string()
    }
}

/// Handle one inbound ACP request. Free function (not a method) so tests can drive a single
/// request against an in-memory connection and assert the exact frames written.
pub async fn handle_request(
    conn: &Arc<AcpConnection>,
    sessions: &Arc<AcpSessions>,
    transcript: &TranscriptSlot,
    tx: &Arc<RequestSender>,
    id: Value,
    method: &str,
    params: Value,
) {
    match method {
        protocol::M_INITIALIZE => {
            let _ = conn.respond_ok(&id, protocol::initialize_result()).await;
        }
        protocol::M_SESSION_NEW => {
            let (acp_id, _) = sessions.new_session();
            if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
                sessions.set_cwd(&acp_id, Some(cwd.to_string()));
            }
            let _ = conn.respond_ok(&id, protocol::session_result(&acp_id, sessions.mode(&acp_id))).await;
        }
        protocol::M_SESSION_LOAD => {
            // REPLAY the transcript via session/update BEFORE returning the result — the ACP
            // spec's ordering requirement (the editor rebuilds the conversation, then the load
            // resolves). `fork`/`resume` variants share this path.
            let client_id = params.get("sessionId").and_then(Value::as_str);
            let acp_id = match params.get("fork").and_then(Value::as_bool) {
                Some(true) => {
                    let (fork_id, _) = sessions.fork(client_id.unwrap_or(""));
                    fork_id
                }
                _ => {
                    let acp_id = client_id.map(str::to_string).unwrap_or_else(|| sessions.new_session().0);
                    sessions.attach(&acp_id, None);
                    acp_id
                }
            };
            if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
                sessions.set_cwd(&acp_id, Some(cwd.to_string()));
            }
            replay_transcript(conn, sessions, transcript, &acp_id, client_id.unwrap_or(&acp_id)).await;
            let _ = conn.respond_ok(&id, protocol::session_result(&acp_id, sessions.mode(&acp_id))).await;
        }
        protocol::M_SESSION_SET_MODE => {
            if let (Some(sid), Some(mode_id)) = (
                params.get("sessionId").and_then(Value::as_str),
                params.get("modeId").and_then(Value::as_str),
            ) {
                sessions.set_mode(sid, SessionMode::from_id(mode_id));
            }
            let _ = conn.respond_ok(&id, json!({})).await;
        }
        protocol::M_SESSION_PROMPT => {
            handle_prompt(conn, sessions, tx, id, params).await;
        }
        "session/list" => {
            let ids: Vec<Value> = sessions.list().into_iter().map(Value::String).collect();
            let _ = conn.respond_ok(&id, json!({ "sessions": ids })).await;
        }
        other => {
            let _ = conn.respond_err(&id, jsonrpc::METHOD_NOT_FOUND, &format!("unknown method: {other}")).await;
        }
    }
}

/// Emit one `session/update` per persisted transcript entry (oldest first), before the caller
/// resolves the `session/load`. Empty (and a no-op) when no provider is injected.
async fn replay_transcript(
    conn: &Arc<AcpConnection>,
    sessions: &Arc<AcpSessions>,
    transcript: &TranscriptSlot,
    acp_id: &str,
    source_session: &str,
) {
    let provider = transcript.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(provider) = provider else { return };
    // Replay from the source session's Haily id (its stored messages), rendered under the
    // (possibly forked) ACP id the editor now holds.
    let lookup_id = sessions
        .haily_id(source_session)
        .map(|u| u.to_string())
        .unwrap_or_else(|| source_session.to_string());
    let entries = provider.transcript(&lookup_id).await;
    for update in protocol::transcript_updates(acp_id, &entries) {
        let _ = conn.notify(protocol::M_SESSION_UPDATE, update).await;
    }
}

/// Handle `session/prompt`: tag-strip the editor-supplied text, forward it as a Chat-origin
/// `Request`, stream the turn's chunks back via `deliver()` (a separate path), and respond only
/// once the turn completes. Queue-and-drain: sequential turns per session are serialized by the
/// completion rendezvous (a new turn's chunks route by session id).
async fn handle_prompt(
    conn: &Arc<AcpConnection>,
    sessions: &Arc<AcpSessions>,
    tx: &Arc<RequestSender>,
    id: Value,
    params: Value,
) {
    let Some(acp_id) = params.get("sessionId").and_then(Value::as_str) else {
        let _ = conn.respond_err(&id, jsonrpc::INVALID_PARAMS, "missing sessionId").await;
        return;
    };
    let haily_id = sessions.attach(acp_id, None);
    let message = extract_prompt_text(&params);
    if message.trim().is_empty() {
        let _ = conn.respond_err(&id, jsonrpc::INVALID_PARAMS, "empty prompt").await;
        return;
    }

    // Register the turn-complete rendezvous BEFORE sending, so a fast Complete cannot be missed.
    let done = conn.register_completion(haily_id);
    let req = protocol::build_prompt_request(haily_id, message, None);
    if tx.send(req).await.is_err() {
        let _ = conn.respond_err(&id, jsonrpc::INTERNAL_ERROR, "orchestrator unavailable").await;
        return;
    }
    // Wait for the turn to finish (Complete fires `done` from `deliver`).
    let _ = done.await;
    let _ = conn.respond_ok(&id, json!({ "stopReason": "end_turn" })).await;
}

/// Handle a client notification. `session/cancel` is the safety-critical one: it drains and
/// DENIES every pending approval for the session (fail-safe) so an in-flight destructive action
/// is never left waiting after the editor asked to stop.
fn handle_notification(conn: &Arc<AcpConnection>, sessions: &Arc<AcpSessions>, method: &str, params: &Value) {
    if method == protocol::M_SESSION_CANCEL {
        if let Some(acp_id) = params.get("sessionId").and_then(Value::as_str) {
            // Fire every pending approval's cancel token → each awaiting `request_permission`
            // wakes immediately and resolves its gate as DENIED (fail-safe, no timeout wait).
            let n = sessions.cancel_pending(acp_id);
            if n > 0 {
                tracing::info!("acp: session/cancel denied {n} pending approval(s)");
            }
            // Also wake any prompt handler still awaiting turn completion.
            if let Some(haily_id) = sessions.haily_id(acp_id) {
                conn.signal_complete(&haily_id);
            }
        }
    } else {
        tracing::debug!("acp: ignoring unhandled notification {method}");
    }
}

/// Extract prompt text from `session/prompt` params and TAG-STRIP it — editor-supplied content
/// is untrusted and must not carry a live `<tool_call>`/`<tool_result>` token to the model.
/// Accepts either a plain `{"prompt":"..."}` or the ACP content-block array
/// `{"prompt":[{"type":"text","text":"..."}]}`.
fn extract_prompt_text(params: &Value) -> String {
    let raw = match params.get("prompt") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => params.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
    };
    crate::run_event::strip_tool_tags_public(&raw)
}

#[cfg(test)]
mod tests;
