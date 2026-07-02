pub mod cli;
pub mod gui;
pub mod manager;

#[cfg(feature = "telegram")]
pub mod telegram;

pub use cli::CliAdapter;
pub use gui::{GuiAdapter, GuiRequestSender, GuiResponseReceiver};
pub use manager::AdapterManager;

#[cfg(feature = "telegram")]
pub use telegram::TelegramAdapter;

// Message/DTO types live in `haily-types` (leaf crate) so `haily-core` can depend on them
// without importing this adapter layer. Re-exported here so existing call sites
// (haily-cli, src-tauri, haily-proactive) need no import changes.
pub use haily_types::{Notification, Request, RequestSender, ResponseChunk, WorkItemStatus};

use anyhow::Result;
use async_trait::async_trait;
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

    fn id(&self) -> &str;
}
