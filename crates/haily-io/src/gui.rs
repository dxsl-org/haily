use crate::{Adapter, Notification, Request, RequestSender, ResponseChunk, WorkItemStatus};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Mutex};
use uuid::Uuid;

/// Channel type the Tauri command handlers write into to send user messages.
pub type GuiRequestSender = mpsc::Sender<Request>;
/// Channel type the Tauri app reads from to receive response chunks.
pub type GuiResponseReceiver = mpsc::Receiver<(Uuid, ResponseChunk)>;
/// Channel type the Tauri app reads from to receive live work-item snapshots.
///
/// A `watch` channel, not an `mpsc`, by design (Phase 5 GUI panel, M-finding): it is a
/// single-slot latest-wins cell — `send_replace` always succeeds and overwrites
/// whatever snapshot hasn't been consumed yet, so a burst of updates during token
/// streaming can never back up or block `notify()` the way a full bounded `mpsc`
/// would. This is intentionally a SEPARATE channel from `resp_tx` (cap 256) so
/// work-item bursts can never compete with chat chunks for that channel's capacity.
/// Because an intermediate snapshot can be silently dropped (only the latest survives),
/// consumers MUST treat every value here as a best-effort delta and reconcile via the
/// `list_work_items` command on (re)mount — see `WorkItemsPanel.svelte`. Phase 08
/// reuses this same channel/policy for its own proactive kinds.
pub type GuiWorkItemsReceiver = watch::Receiver<Vec<WorkItemStatus>>;

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
    /// Latest-wins live forward of `WorkItemsChanged` snapshots — see `GuiWorkItemsReceiver`.
    work_items_tx: watch::Sender<Vec<WorkItemStatus>>,
}

impl GuiAdapter {
    /// Creates the adapter and returns the channel endpoints the Tauri side needs.
    pub fn new() -> (Self, GuiRequestSender, GuiResponseReceiver, GuiWorkItemsReceiver) {
        let (req_tx, req_rx) = mpsc::channel::<Request>(64);
        let (resp_tx, resp_rx) = mpsc::channel::<(Uuid, ResponseChunk)>(256);
        let (work_items_tx, work_items_rx) = watch::channel(Vec::new());

        let adapter = GuiAdapter {
            req_rx: Arc::new(Mutex::new(req_rx)),
            resp_tx,
            work_items_tx,
        };

        (adapter, req_tx, resp_rx, work_items_rx)
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
        // WorkItemsChanged is forwarded over the dedicated latest-wins `work_items_tx`
        // watch channel (see `GuiWorkItemsReceiver`), never through `resp_tx` — the
        // work-items panel is not a chat bubble, and routing it through the bounded
        // chat channel would let a work-item burst compete with (and potentially
        // starve behind) in-flight response chunks. `send_replace` cannot block or
        // fail, so this can never stall the daemon regardless of whether a frontend
        // is currently listening.
        if let Notification::WorkItemsChanged(items) = msg {
            self.work_items_tx.send_replace(items);
            return Ok(());
        }
        let text = match &msg {
            Notification::MorningBrief(brief) => format!("[Morning Brief]\n{brief}"),
            Notification::Alert { title, body, .. } => format!("{title}\n{body}"),
            Notification::ReminderFired { title, .. } => format!("⏰ {title}"),
            // Unreachable in practice (the early-return above handles it), but the
            // match must be total: a future refactor removing that guard must degrade
            // to a dropped notification, never panic the always-on daemon.
            Notification::WorkItemsChanged(_) => {
                tracing::debug!(
                    "WorkItemsChanged reached notify() text-match; handled upstream — ignoring"
                );
                return Ok(());
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Adapter;

    /// Regression: `WorkItemsChanged` must never panic `notify()`. This is currently
    /// caught by the early-return guard, but the test also protects the match arm
    /// itself (see the comment on that arm) if the guard is ever removed.
    #[tokio::test]
    async fn notify_work_items_changed_does_not_panic() {
        let (adapter, _req_tx, _resp_rx, _wi_rx) = GuiAdapter::new();

        let result = adapter.notify(Notification::WorkItemsChanged(vec![])).await;

        assert!(result.is_ok());
    }

    /// The live-forward channel is latest-wins: a second `WorkItemsChanged` overwrites
    /// the first before it's ever observed, and the receiver always sees only the most
    /// recent snapshot — this is the coalesce/drop policy the phase-5 architecture
    /// note requires (never queue, never block on a full channel).
    #[tokio::test]
    async fn notify_work_items_changed_is_latest_wins() {
        let (adapter, _req_tx, _resp_rx, mut wi_rx) = GuiAdapter::new();
        let first = vec![WorkItemStatus {
            title: "first".into(),
            status: "running".into(),
            progress: 10,
            phase: None,
        }];
        let second = vec![WorkItemStatus {
            title: "second".into(),
            status: "done".into(),
            progress: 100,
            phase: None,
        }];

        adapter.notify(Notification::WorkItemsChanged(first)).await.unwrap();
        adapter.notify(Notification::WorkItemsChanged(second)).await.unwrap();

        wi_rx.changed().await.unwrap();
        let latest = wi_rx.borrow_and_update().clone();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].title, "second");
    }
}
