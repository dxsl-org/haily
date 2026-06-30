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

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub session_id: Uuid,
    pub adapter_id: String,
    pub message: String,
    pub user_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ResponseChunk {
    Text(String),
    ToolApprovalRequest {
        tool: String,
        args: String,
        approval_id: Uuid,
    },
    ToolResult {
        name: String,
        ok: bool,
    },
    Complete,
}

/// Snapshot of a single active work item for display in adapters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItemStatus {
    pub title: String,
    pub status: String,
    pub progress: u8,
    pub phase: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Notification {
    MorningBrief(String),
    Alert {
        title: String,
        body: String,
        urgent: bool,
    },
    ReminderFired {
        reminder_id: Uuid,
        title: String,
    },
    /// Broadcast when the set of active work items changes (added, progressed, or removed).
    WorkItemsChanged(Vec<WorkItemStatus>),
}

pub type RequestSender = tokio::sync::mpsc::Sender<Request>;
pub type RequestReceiver = tokio::sync::mpsc::Receiver<Request>;

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
