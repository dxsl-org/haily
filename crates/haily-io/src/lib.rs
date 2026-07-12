/// ACP (Agent Client Protocol) coding channel — a 4th `Adapter` speaking newline-delimited
/// JSON-RPC 2.0 over stdio (phase 12).
pub mod acp;
pub mod cli;
pub mod gui;
pub mod manager;
/// Internal to this crate — pure card accumulation/eviction logic `gui.rs`'s
/// `Adapter` impl delegates to (phase 08). Not part of the public API surface.
mod proactive_cards;
/// Ordered-`RunEvent` delivery defense — the single tag-strip chokepoint (phase 11a).
pub mod run_event;
/// Canonical per-channel slash-command registry (phase 11a).
pub mod slash;

#[cfg(feature = "telegram")]
pub mod telegram;

/// Desktop mobile-server core (Mobile Thin-Client plan phase 2a) — see `mobile::MobileAdapter`.
#[cfg(feature = "mobile-server")]
pub mod mobile;

pub use acp::AcpAdapter;
pub use cli::CliAdapter;
pub use gui::{
    GuiAdapter, GuiProactiveReceiver, GuiRequestSender, GuiResponseReceiver, GuiRunEventReceiver,
    GuiWorkItemsReceiver,
};
pub use manager::AdapterManager;

#[cfg(feature = "telegram")]
pub use telegram::TelegramAdapter;

// Message/DTO types live in `haily-types` (leaf crate) so `haily-core` can depend on them
// without importing this adapter layer. Re-exported here so existing call sites
// (haily-cli, src-tauri, haily-proactive) need no import changes.
pub use haily_types::{
    ApprovalResolver, DepthMode, Notification, ProactiveCard, ProactiveCardKind, Request,
    RequestOrigin, RequestSender, ResponseChunk, RunEvent, SessionTranscript, TranscriptEntry,
    WorkItemStatus,
};

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use uuid::Uuid;

#[async_trait]
pub trait Adapter: Send + Sync {
    /// Launch the adapter's event loop. Sends incoming user requests via `tx`.
    /// Returns immediately — the event loop runs in a spawned task.
    async fn start(&self, tx: RequestSender) -> Result<()>;

    /// Deliver an orchestrator response chunk to the session's origin.
    async fn deliver(&self, session_id: Uuid, chunk: ResponseChunk) -> Result<()>;

    /// Deliver one ordered pipeline [`RunEvent`] to the session's origin (phase 11a).
    ///
    /// Distinct from [`Self::deliver`] because a coding-pipeline run is a long-lived job
    /// with its own ORDERED, NON-COALESCING event log — it must never drop or reorder
    /// events the way the latest-wins work-item/proactive `watch` channels do. The event
    /// reaches here already tag-stripped ([`crate::AdapterManager::deliver_run_event`] is
    /// the single sanitize chokepoint), so a render path may treat it as inert data.
    ///
    /// Default no-op: a channel with no run-observability surface (or one wired later)
    /// need not override it — same post-construction contract as the other trait methods.
    async fn deliver_run_event(&self, _session_id: Uuid, _event: RunEvent) -> Result<()> {
        Ok(())
    }

    /// Send a proactive notification (morning brief, alert, reminder fired).
    async fn notify(&self, msg: Notification) -> Result<()>;

    /// Inject the tool-approval resolver. Called once by `haily-app::bootstrap`
    /// after the `Orchestrator` (and therefore its broker) exists — adapters are
    /// constructed before that point, so this is a post-construction wiring step,
    /// not a constructor arg. Default no-op: adapters with no interactive approval
    /// surface (or that resolve some other way, e.g. the GUI's direct Tauri command)
    /// don't need to override this.
    fn set_approval_resolver(&self, _resolver: Arc<dyn ApprovalResolver>) {}

    /// Inject the `safety.disable_writes` kill switch (phase 3, C8). Same post-construction
    /// wiring contract as `set_approval_resolver`: called once by `haily-app::bootstrap`
    /// after the orchestrator exists. Default no-op — an adapter with its own toggle
    /// surface (e.g. the GUI's Tauri `set_preference`) does not need this. The CLI overrides
    /// it to power its `/writes on|off` command.
    fn set_kill_switch(&self, _kill: Arc<std::sync::atomic::AtomicBool>) {}

    /// Inject a read-only session-transcript provider (phase 12). Same post-construction
    /// wiring contract as `set_approval_resolver`: `haily-app::bootstrap` calls it once after
    /// the DB exists, with a `haily-db`-backed implementation. Only the ACP adapter overrides
    /// it — it replays a session's transcript on `session/load`. Default no-op: a channel with
    /// no replay surface (GUI, CLI, Telegram) simply never receives one.
    fn set_session_transcript(&self, _transcript: Arc<dyn haily_types::SessionTranscript>) {}

    /// Inject a handle back to the `AdapterManager` that registered this adapter (Mobile
    /// Thin-Client plan phase 2a review fix, red team m7). Same post-construction wiring
    /// contract as `set_approval_resolver`: `manager::AdapterManager::wire_self_reference`
    /// calls this once, right after the manager itself is built (which is necessarily AFTER
    /// every adapter's construction — the manager doesn't exist yet at that point). Default
    /// no-op: only an adapter that itself needs to broadcast a `Notification` to every OTHER
    /// adapter (today: the mobile adapter's kill-switch ENABLE path) needs to override this.
    fn set_adapter_manager(&self, _manager: crate::manager::AdapterManager) {}

    fn id(&self) -> &str;
}
