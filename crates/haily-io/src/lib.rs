pub mod cli;
pub mod gui;
pub mod manager;
/// Internal to this crate — pure card accumulation/eviction logic `gui.rs`'s
/// `Adapter` impl delegates to (phase 08). Not part of the public API surface.
mod proactive_cards;

#[cfg(feature = "telegram")]
pub mod telegram;

pub use cli::CliAdapter;
pub use gui::{
    GuiAdapter, GuiProactiveReceiver, GuiRequestSender, GuiResponseReceiver, GuiWorkItemsReceiver,
};
pub use manager::AdapterManager;

#[cfg(feature = "telegram")]
pub use telegram::TelegramAdapter;

// Message/DTO types live in `haily-types` (leaf crate) so `haily-core` can depend on them
// without importing this adapter layer. Re-exported here so existing call sites
// (haily-cli, src-tauri, haily-proactive) need no import changes.
pub use haily_types::{
    ApprovalResolver, DepthMode, Notification, ProactiveCard, ProactiveCardKind, Request,
    RequestSender, ResponseChunk, WorkItemStatus,
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

    fn id(&self) -> &str;
}
