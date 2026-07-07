use crate::proactive_cards::upsert_proactive_card;
use crate::{
    Adapter, Notification, ProactiveCard, Request, RequestSender, ResponseChunk, WorkItemStatus,
};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Mutex};
use uuid::Uuid;

/// Decision (phase 08): the old NIL-session chat bubble for `MorningBrief`/`Alert`/
/// `ReminderFired` is now redundant with the typed `ProactivePanel` card surface, and
/// the red-team direction was to prefer it suppressed now that those kinds "have a
/// home". Kept as a single flag (rather than deleting the fallback code below) so
/// re-enabling it — if the panel turns out to drop/misrender events in practice — is
/// a one-line change, not a re-implementation; this is the "smallest reversible
/// option" for a UI surface that has no automated regression for visual correctness.
const PROACTIVE_CHAT_BUBBLE_FALLBACK: bool = false;

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
/// `list_work_items` command on (re)mount — see `WorkItemsPanel.svelte`.
pub type GuiWorkItemsReceiver = watch::Receiver<Vec<WorkItemStatus>>;

/// Channel type the Tauri app reads from to receive live proactive-card snapshots
/// (phase 08). Same `watch`-channel discipline as `GuiWorkItemsReceiver` and the SAME
/// rationale — a burst of proactive events during a busy daemon tick can never back
/// up or block `notify()`. It differs in one respect: `WorkItemsChanged` is a single
/// full-snapshot replacement, whereas proactive kinds are discrete events, so this
/// value is itself already an ACCUMULATED, per-kind-capped list (see
/// `upsert_proactive_card`) rather than "whatever `notify()` was last called with" —
/// the accumulation happens on the `GuiAdapter` side specifically so a dropped
/// intermediate `watch` value (frontend busy/not yet listening) does not silently
/// lose an event the way a bare "latest notification only" channel would. There is no
/// `list_*` reconcile command for this surface (unlike work-items) — delivery is
/// best-effort by design; see the phase's Architecture note.
pub type GuiProactiveReceiver = watch::Receiver<Vec<ProactiveCard>>;

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
    /// Accumulated, per-kind-capped live forward of proactive events — see `GuiProactiveReceiver`.
    proactive_tx: watch::Sender<Vec<ProactiveCard>>,
}

impl GuiAdapter {
    /// Creates the adapter and returns the channel endpoints the Tauri side needs.
    pub fn new() -> (
        Self,
        GuiRequestSender,
        GuiResponseReceiver,
        GuiWorkItemsReceiver,
        GuiProactiveReceiver,
    ) {
        let (req_tx, req_rx) = mpsc::channel::<Request>(64);
        let (resp_tx, resp_rx) = mpsc::channel::<(Uuid, ResponseChunk)>(256);
        let (work_items_tx, work_items_rx) = watch::channel(Vec::new());
        let (proactive_tx, proactive_rx) = watch::channel(Vec::new());

        let adapter = GuiAdapter {
            req_rx: Arc::new(Mutex::new(req_rx)),
            resp_tx,
            work_items_tx,
            proactive_tx,
        };

        (adapter, req_tx, resp_rx, work_items_rx, proactive_rx)
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

        // Forward the typed card over its own watch channel (see `GuiProactiveReceiver`)
        // BEFORE the (now-suppressed-by-default) text fallback below — this is the
        // structured card surface phase 08 adds. `borrow()` + `send_replace` never
        // blocks or fails, matching the work-items channel's never-stall-the-daemon
        // guarantee.
        if let Some(card) = ProactiveCard::from_notification(&msg) {
            let updated = upsert_proactive_card(&self.proactive_tx.borrow(), card);
            self.proactive_tx.send_replace(updated);
        }

        if !PROACTIVE_CHAT_BUBBLE_FALLBACK {
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
    use crate::{Adapter, ProactiveCardKind};

    /// Regression: `WorkItemsChanged` must never panic `notify()`. This is currently
    /// caught by the early-return guard, but the test also protects the match arm
    /// itself (see the comment on that arm) if the guard is ever removed.
    #[tokio::test]
    async fn notify_work_items_changed_does_not_panic() {
        let (adapter, _req_tx, _resp_rx, _wi_rx, _pc_rx) = GuiAdapter::new();

        let result = adapter.notify(Notification::WorkItemsChanged(vec![])).await;

        assert!(result.is_ok());
    }

    /// The live-forward channel is latest-wins: a second `WorkItemsChanged` overwrites
    /// the first before it's ever observed, and the receiver always sees only the most
    /// recent snapshot — this is the coalesce/drop policy the phase-5 architecture
    /// note requires (never queue, never block on a full channel).
    #[tokio::test]
    async fn notify_work_items_changed_is_latest_wins() {
        let (adapter, _req_tx, _resp_rx, mut wi_rx, _pc_rx) = GuiAdapter::new();
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

    /// Regression: none of the three discrete proactive kinds may panic `notify()` —
    /// mirrors the earlier `WorkItemsChanged unreachable!()` incident, generalized to
    /// every kind this phase adds a forwarding path for.
    #[tokio::test]
    async fn notify_discrete_proactive_kinds_do_not_panic() {
        let (adapter, _req_tx, _resp_rx, _wi_rx, _pc_rx) = GuiAdapter::new();

        for msg in [
            Notification::MorningBrief("brief".into()),
            Notification::Alert { title: "t".into(), body: "b".into(), urgent: true },
            Notification::ReminderFired { reminder_id: Uuid::new_v4(), title: "call".into() },
        ] {
            assert!(adapter.notify(msg).await.is_ok());
        }
    }

    /// Decision (phase 08): the chat-bubble fallback defaults OFF — a proactive
    /// notification must reach the card channel and must NOT also produce a
    /// `resp_tx` chat bubble (which would double-surface the same event).
    #[tokio::test]
    async fn notify_proactive_forwards_card_and_suppresses_chat_bubble() {
        let (adapter, _req_tx, mut resp_rx, _wi_rx, mut pc_rx) = GuiAdapter::new();

        adapter
            .notify(Notification::Alert { title: "t".into(), body: "b".into(), urgent: true })
            .await
            .unwrap();

        pc_rx.changed().await.unwrap();
        let cards = pc_rx.borrow_and_update().clone();
        assert_eq!(cards.len(), 1);
        assert!(matches!(cards[0].kind, ProactiveCardKind::Alert { urgent: true, .. }));

        assert!(matches!(resp_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
    }

    /// A burst of `Alert`s must never evict a still-present `MorningBrief` card — the
    /// two kinds live in separate eviction buckets. This is an end-to-end check
    /// through `notify()`; `crate::proactive_cards`'s own tests cover the pure
    /// eviction/cap logic in isolation.
    #[tokio::test]
    async fn alert_burst_does_not_evict_morning_brief() {
        let (adapter, _req_tx, _resp_rx, _wi_rx, mut pc_rx) = GuiAdapter::new();

        adapter.notify(Notification::MorningBrief("today's plan".into())).await.unwrap();
        for i in 0..(crate::proactive_cards::MAX_ALERT_CARDS + 5) {
            adapter
                .notify(Notification::Alert { title: format!("a{i}"), body: "b".into(), urgent: false })
                .await
                .unwrap();
        }

        pc_rx.changed().await.unwrap();
        let cards = pc_rx.borrow_and_update().clone();
        let briefs = cards
            .iter()
            .filter(|c| matches!(c.kind, ProactiveCardKind::MorningBrief { .. }))
            .count();
        let alerts = cards.iter().filter(|c| matches!(c.kind, ProactiveCardKind::Alert { .. })).count();
        assert_eq!(briefs, 1, "morning brief must survive an unrelated alert burst");
        assert_eq!(
            alerts,
            crate::proactive_cards::MAX_ALERT_CARDS,
            "alert bucket must be capped independently"
        );
    }
}
