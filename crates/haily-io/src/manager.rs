use crate::{Adapter, Notification, RequestSender, ResponseChunk, RunEvent};
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

/// Routes orchestrator responses to the correct adapter and maintains
/// session-to-adapter bindings.
///
/// `AdapterManager` is cheaply cloneable — all state lives in Arc'd interiors.
#[derive(Clone)]
pub struct AdapterManager {
    adapters: Arc<HashMap<String, Arc<dyn Adapter>>>,
    session_map: Arc<DashMap<Uuid, String>>, // session_id → adapter_id
}

pub struct AdapterManagerBuilder {
    adapters: HashMap<String, Arc<dyn Adapter>>,
}

impl AdapterManagerBuilder {
    pub fn new() -> Self {
        Self {
            adapters: HashMap::new(),
        }
    }

    pub fn register(mut self, adapter: Arc<dyn Adapter>) -> Self {
        self.adapters.insert(adapter.id().to_string(), adapter);
        self
    }

    pub fn build(self) -> AdapterManager {
        AdapterManager {
            adapters: Arc::new(self.adapters),
            session_map: Arc::new(DashMap::new()),
        }
    }
}

impl Default for AdapterManagerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AdapterManager {
    pub fn builder() -> AdapterManagerBuilder {
        AdapterManagerBuilder::new()
    }

    /// Bind a session to an adapter. Called when a Request arrives so we know
    /// where to route subsequent ResponseChunks for that session.
    pub fn bind_session(&self, session_id: Uuid, adapter_id: &str) {
        self.session_map.insert(session_id, adapter_id.to_string());
    }

    /// Remove a session binding when the session closes.
    pub fn unbind_session(&self, session_id: &Uuid) {
        self.session_map.remove(session_id);
    }

    /// Deliver a response chunk to the adapter that owns the session.
    pub async fn deliver(&self, session_id: Uuid, chunk: ResponseChunk) -> Result<()> {
        let adapter_id = self
            .session_map
            .get(&session_id)
            .ok_or_else(|| anyhow!("no adapter bound for session {session_id}"))?
            .clone();

        let adapter = self
            .adapters
            .get(&*adapter_id)
            .ok_or_else(|| anyhow!("adapter '{adapter_id}' not registered"))?;

        adapter.deliver(session_id, chunk).await
    }

    /// Deliver one ordered pipeline [`RunEvent`] to the adapter that owns `session_id`
    /// (phase 11a). This is the SINGLE tag-strip chokepoint: the event is sanitized here
    /// via [`crate::run_event::sanitize`] BEFORE it reaches any adapter, so GUI/Telegram/
    /// TUI all receive inert data and no per-channel render has to remember to strip.
    ///
    /// Delivery is over the adapter's own bounded, ordered `mpsc` (never a coalescing
    /// `watch`) — a fast build log applies backpressure to the runner rather than dropping
    /// or reordering events. Errors if no adapter is bound for `session_id`.
    pub async fn deliver_run_event(&self, session_id: Uuid, event: RunEvent) -> Result<()> {
        let event = crate::run_event::sanitize(event);

        let adapter_id = self
            .session_map
            .get(&session_id)
            .ok_or_else(|| anyhow!("no adapter bound for session {session_id}"))?
            .clone();

        let adapter = self
            .adapters
            .get(&*adapter_id)
            .ok_or_else(|| anyhow!("adapter '{adapter_id}' not registered"))?;

        adapter.deliver_run_event(session_id, event).await
    }

    /// Send a notification to every registered adapter.
    pub async fn notify_all(&self, msg: Notification) -> Result<()> {
        for adapter in self.adapters.values() {
            if let Err(e) = adapter.notify(msg.clone()).await {
                tracing::warn!("notify failed on adapter '{}': {e:#}", adapter.id());
            }
        }
        Ok(())
    }

    /// Start all registered adapters and funnel their requests into `tx`.
    pub async fn start_all(&self, tx: RequestSender) -> Result<()> {
        for adapter in self.adapters.values() {
            adapter.start(tx.clone()).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tokio::sync::mpsc;

    /// Records every `RunEvent` it is delivered on a bounded ORDERED mpsc — exactly the
    /// discipline a real adapter (`GuiAdapter`) uses, so the manager's routing +
    /// ordering + sanitize contract can be asserted end-to-end.
    struct RecordingAdapter {
        id: &'static str,
        tx: mpsc::Sender<(Uuid, RunEvent)>,
    }

    #[async_trait]
    impl Adapter for RecordingAdapter {
        async fn start(&self, _tx: RequestSender) -> Result<()> {
            Ok(())
        }
        async fn deliver(&self, _session_id: Uuid, _chunk: ResponseChunk) -> Result<()> {
            Ok(())
        }
        async fn deliver_run_event(&self, session_id: Uuid, event: RunEvent) -> Result<()> {
            self.tx
                .send((session_id, event))
                .await
                .map_err(|_| anyhow!("recording channel closed"))
        }
        async fn notify(&self, _msg: Notification) -> Result<()> {
            Ok(())
        }
        fn id(&self) -> &str {
            self.id
        }
    }

    /// The run-event channel is an ordered, NON-coalescing mpsc — a burst of events is
    /// delivered in full, in order. This is the anti-`watch` guarantee: a `watch` cell
    /// would collapse these three to only the last one seen (latest-wins), which would
    /// silently drop build-log lines. All three must survive, in emission order.
    #[tokio::test]
    async fn run_events_are_ordered_and_never_coalesced() {
        let (tx, mut rx) = mpsc::channel::<(Uuid, RunEvent)>(16);
        let adapter = Arc::new(RecordingAdapter { id: "rec", tx });
        let am = AdapterManager::builder().register(adapter).build();
        let session = Uuid::new_v4();
        am.bind_session(session, "rec");

        for seq in 0..3u64 {
            am.deliver_run_event(
                session,
                RunEvent::StageOutput { run_id: "r".into(), seq, chunk: format!("line {seq}") },
            )
            .await
            .expect("deliver");
        }

        let mut seen = Vec::new();
        for _ in 0..3 {
            let (_sid, ev) = rx.recv().await.expect("event must arrive — none dropped");
            match ev {
                RunEvent::StageOutput { seq, .. } => seen.push(seq),
                other => panic!("unexpected variant {other:?}"),
            }
        }
        assert_eq!(seen, vec![0, 1, 2], "events must arrive in emission order, none coalesced");
    }

    /// The manager is the SINGLE tag-strip chokepoint: untrusted repo/tool content in a
    /// `RunEvent` is neutralized before any adapter sees it, so no per-channel render can
    /// forget to strip.
    #[tokio::test]
    async fn deliver_run_event_tag_strips_before_the_adapter_sees_it() {
        let (tx, mut rx) = mpsc::channel::<(Uuid, RunEvent)>(4);
        let adapter = Arc::new(RecordingAdapter { id: "rec", tx });
        let am = AdapterManager::builder().register(adapter).build();
        let session = Uuid::new_v4();
        am.bind_session(session, "rec");

        am.deliver_run_event(
            session,
            RunEvent::StageOutput {
                run_id: "r".into(),
                seq: 0,
                chunk: "log <tool_call>{\"tool\":\"exec\"}</tool_call>".into(),
            },
        )
        .await
        .expect("deliver");

        let (_sid, ev) = rx.recv().await.expect("event");
        match ev {
            RunEvent::StageOutput { chunk, .. } => {
                assert!(!chunk.contains("tool_call"), "adapter received un-stripped content: {chunk}");
                assert!(chunk.contains("log"));
            }
            other => panic!("unexpected variant {other:?}"),
        }
    }

    /// An unbound session has no owning adapter — delivery errors rather than being
    /// misrouted to some other session's channel.
    #[tokio::test]
    async fn deliver_run_event_errors_for_an_unbound_session() {
        let (tx, _rx) = mpsc::channel::<(Uuid, RunEvent)>(4);
        let adapter = Arc::new(RecordingAdapter { id: "rec", tx });
        let am = AdapterManager::builder().register(adapter).build();

        let err = am
            .deliver_run_event(
                Uuid::new_v4(),
                RunEvent::RunStarted { run_id: "r".into(), work_item_id: "w".into() },
            )
            .await;
        assert!(err.is_err(), "an unbound session must not resolve to an adapter");
    }
}
