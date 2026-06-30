use crate::{Adapter, Notification, Request, RequestSender, ResponseChunk};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

/// Channel type the Tauri command handlers write into to send user messages.
pub type GuiRequestSender = mpsc::Sender<Request>;
/// Channel type the Tauri app reads from to receive response chunks.
pub type GuiResponseReceiver = mpsc::Receiver<(Uuid, ResponseChunk)>;

/// Tauri IPC bridge. No Tauri dependency lives here — communication is via channels.
///
/// Tauri side wiring (Phase 10):
/// - User message arrives as Tauri command → write to `gui_req_tx`
/// - Response chunk arrives in `gui_resp_rx` → emit as Tauri event to frontend
pub struct GuiAdapter {
    /// Receives requests from Tauri commands
    req_rx: Arc<Mutex<mpsc::Receiver<Request>>>,
    /// Delivers response chunks back to the Tauri app
    resp_tx: mpsc::Sender<(Uuid, ResponseChunk)>,
}

impl GuiAdapter {
    /// Creates the adapter and returns the two channel endpoints the Tauri side needs.
    pub fn new() -> (Self, GuiRequestSender, GuiResponseReceiver) {
        let (req_tx, req_rx) = mpsc::channel::<Request>(64);
        let (resp_tx, resp_rx) = mpsc::channel::<(Uuid, ResponseChunk)>(256);

        let adapter = GuiAdapter {
            req_rx: Arc::new(Mutex::new(req_rx)),
            resp_tx,
        };

        (adapter, req_tx, resp_rx)
    }
}

#[async_trait]
impl Adapter for GuiAdapter {
    /// Drains requests from the Tauri channel and forwards them to the orchestrator.
    async fn start(&self, tx: RequestSender) -> Result<()> {
        let req_rx = Arc::clone(&self.req_rx);

        tokio::spawn(async move {
            let mut rx = req_rx.lock().await;
            while let Some(req) = rx.recv().await {
                if tx.send(req).await.is_err() {
                    break;
                }
            }
        });

        Ok(())
    }

    async fn deliver(&self, session_id: Uuid, chunk: ResponseChunk) -> Result<()> {
        self.resp_tx
            .send((session_id, chunk))
            .await
            .map_err(|_| anyhow::anyhow!("GUI response channel closed"))?;
        Ok(())
    }

    async fn notify(&self, msg: Notification) -> Result<()> {
        // Buffer as a synthetic "system" response chunk so the UI can display it.
        let text = match &msg {
            Notification::MorningBrief(brief) => format!("[Morning Brief]\n{brief}"),
            Notification::Alert { title, body, .. } => format!("{title}\n{body}"),
            Notification::ReminderFired { title, .. } => format!("⏰ {title}"),
        };
        // Delivered on a synthetic session so Phase 10 can route it to a notification panel.
        let synthetic_session = Uuid::nil();
        self.resp_tx
            .send((synthetic_session, ResponseChunk::Text(text)))
            .await
            .ok();
        self.resp_tx
            .send((synthetic_session, ResponseChunk::Complete))
            .await
            .ok();
        Ok(())
    }

    fn id(&self) -> &str {
        "gui"
    }
}
