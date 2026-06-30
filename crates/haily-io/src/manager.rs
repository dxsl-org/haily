use crate::{Adapter, Notification, RequestSender, ResponseChunk};
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
        Self { adapters: HashMap::new() }
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
