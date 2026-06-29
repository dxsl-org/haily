mod agent;
mod context;
pub mod feedback_parser;
mod tool_call;

use anyhow::Result;
use haily_db::DbHandle;
use haily_io::{Request, ResponseChunk};
use haily_kms::KmsHandle;
use haily_llm::{LlmConfig, LlmRouter};
use haily_tools::ToolRegistry;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

pub struct Orchestrator {
    pub kms: Arc<KmsHandle>,
    pub db: Arc<DbHandle>,
    /// RwLock allows swapping the LLM without restarting the app.
    /// Lock is held only for the duration of cloning the inner Arc — never across await.
    llm: Arc<RwLock<Arc<LlmRouter>>>,
    tools: Arc<ToolRegistry>,
}

impl Orchestrator {
    pub async fn init(
        kms: Arc<KmsHandle>,
        db: Arc<DbHandle>,
        config: LlmConfig,
    ) -> Result<Self> {
        let llm_inner = Arc::new(LlmRouter::init(config).await);
        let tools = Arc::new(ToolRegistry::build_v1());
        tracing::info!(
            llm = llm_inner.provider_name(),
            tools = tools.len(),
            "Orchestrator ready"
        );

        Self::spawn_self_improvement_workers(Arc::clone(&kms), Arc::clone(&llm_inner));
        let llm = Arc::new(RwLock::new(llm_inner));

        Ok(Self { kms, db, llm, tools })
    }

    /// Swap in a new LLM backend without restarting. Safe to call while requests are in flight.
    pub async fn reload_llm(&self, config: LlmConfig) {
        let new_router = Arc::new(LlmRouter::init(config).await);
        tracing::info!(llm = new_router.provider_name(), "LLM reloaded");
        *self.llm.write().expect("llm lock") = new_router;
    }

    pub async fn process(
        &self,
        req: Request,
        tx: mpsc::Sender<ResponseChunk>,
    ) -> Result<()> {
        // Clone the Arc under a brief read-lock — never hold the lock across await.
        let llm = self.llm.read().expect("llm lock").clone();
        agent::run_turn(
            &req,
            Arc::clone(&self.db),
            Arc::clone(&self.kms),
            llm,
            Arc::clone(&self.tools),
            tx,
        )
        .await
    }

    pub fn llm_provider(&self) -> String {
        self.llm.read().expect("llm lock").provider_name().to_string()
    }

    /// Spawn background workers for skill synthesis (hourly) and decay (daily).
    fn spawn_self_improvement_workers(kms: Arc<KmsHandle>, llm: Arc<LlmRouter>) {
        // Skill synthesis — every 1 hour
        let kms_s = Arc::clone(&kms);
        let llm_s = Arc::clone(&llm);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                match kms_s.synthesize_skills(llm_s.as_ref()).await {
                    Ok(v) if !v.is_empty() => tracing::info!(count = v.len(), "skills synthesized"),
                    Err(e) => tracing::warn!("skill synthesis failed: {e:#}"),
                    _ => {}
                }
            }
        });

        // Skill decay — every 24 hours
        let kms_d = Arc::clone(&kms);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
                if let Err(e) = kms_d.decay_skills().await {
                    tracing::warn!("skill decay failed: {e:#}");
                }
            }
        });
    }
}
