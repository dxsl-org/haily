mod agent;
mod context;
mod delegate;
mod domains;
pub mod feedback_parser;
mod specialists;
mod tool_call;
pub mod worktree;

use anyhow::Result;
use haily_db::DbHandle;
use haily_types::{Request, ResponseChunk};
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

        // `base_v1` is a clean V1 registry used only for sub_registry() lookups.
        // L0 only exposes a minimal quick-action tool set so weak local models
        // stay well within a manageable tool count (~9 tools + 6 delegates = 15 total).
        // Complex or domain-specific work is handled by L1 agents via delegation.
        const L0_QUICK_TOOLS: &[&str] = &[
            "web_search",       // quick fact lookup
            "memory_search",    // recall personal context
            "memory_remember",  // save a quick note to memory
            "reminder_add",     // set a one-shot reminder
            "calendar_list",    // check upcoming schedule
            "note_save",        // save a quick note
            "work_item_list",   // check active/interrupted tasks
            "work_item_resume", // resume a task
            "feedback_react",   // apply in-line user feedback
        ];
        let base_v1 = ToolRegistry::build_v1();
        let mut tools = base_v1.sub_registry(L0_QUICK_TOOLS);

        // Build one DelegateTool per domain (L0 → L1).
        // Each domain's sub-registry contains:
        //   - the domain's whitelisted V1 tools
        //   - one DelegateTool per L2 specialist belonging to that domain
        for domain in domains::DOMAINS {
            let mut l1_reg = base_v1.sub_registry(domain.allowed_tools);

            // Add L2 specialist delegates into the L1 sub-registry.
            for spec in specialists::SPECIALISTS
                .iter()
                .filter(|s| s.parent_domain == domain.tool_name)
            {
                let l2_reg = Arc::new(base_v1.sub_registry(spec.allowed_tools));
                l1_reg.register(Arc::new(delegate::DelegateTool {
                    tool_name: spec.tool_name,
                    description: spec.description,
                    system_prompt: spec.system_prompt,
                    domain_name: spec.tool_name.trim_start_matches("delegate_to_"),
                    db: Arc::clone(&db),
                    kms: Arc::clone(&kms),
                    llm: Arc::clone(&llm_inner),
                    sub_registry: l2_reg,
                    max_depth: 2, // L1 (depth=1) can spawn; L2 (depth=2) blocked by depth guard
                }));
            }

            tools.register(Arc::new(delegate::DelegateTool {
                tool_name: domain.tool_name,
                description: domain.description,
                system_prompt: domain.system_prompt,
                domain_name: domain.tool_name.trim_start_matches("delegate_to_"),
                db: Arc::clone(&db),
                kms: Arc::clone(&kms),
                llm: Arc::clone(&llm_inner),
                sub_registry: Arc::new(l1_reg),
                max_depth: 1, // L0 (depth=0) can spawn L1; L1 depth guard handles the rest
            }));
        }

        let tools = Arc::new(tools);
        tracing::info!(
            llm = llm_inner.provider_name(),
            tools = tools.len(),
            "Orchestrator ready"
        );

        // Reset work items stuck in `running` from a previous crash to `interrupted`.
        match haily_db::queries::work_items::reset_stale_running(&db).await {
            Ok(n) if n > 0 => tracing::info!(count = n, "work items reset to interrupted"),
            Err(e) => tracing::warn!("failed to reset stale work items: {e:#}"),
            _ => {}
        }

        Self::spawn_self_improvement_workers(Arc::clone(&kms), Arc::clone(&llm_inner));
        let llm = Arc::new(RwLock::new(llm_inner));

        Ok(Self { kms, db, llm, tools })
    }

    /// Swap in a new LLM backend without restarting. Safe to call while requests are in flight.
    pub async fn reload_llm(&self, config: LlmConfig) {
        let new_router = Arc::new(LlmRouter::init(config).await);
        tracing::info!(llm = new_router.provider_name(), "LLM reloaded");
        // Recover from a poisoned lock rather than panicking: the guarded value is an
        // `Arc` clone with no partial state, so a prior panicking holder cannot have
        // left it inconsistent — taking the inner value and continuing is safe.
        let mut guard = self.llm.write().unwrap_or_else(|e| e.into_inner());
        *guard = new_router;
    }

    pub async fn process(
        &self,
        req: Request,
        tx: mpsc::Sender<ResponseChunk>,
    ) -> Result<()> {
        // Clone the Arc under a brief read-lock — never hold the lock across await.
        let llm = self.llm.read().unwrap_or_else(|e| e.into_inner()).clone();
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
        self.llm.read().unwrap_or_else(|e| e.into_inner()).provider_name().to_string()
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

#[cfg(test)]
mod wiring_tests {
    //! Guards the 3-tier registry wiring against silent drift. `sub_registry`
    //! silently drops unknown tool names, so a typo in any `allowed_tools` entry
    //! would strip a capability with zero runtime signal — these tests turn that
    //! into a compile-then-test failure instead.
    use crate::{domains::DOMAINS, specialists::SPECIALISTS};
    use haily_tools::ToolRegistry;

    #[test]
    fn all_domain_whitelists_resolve() {
        let base = ToolRegistry::build_v1();
        for d in DOMAINS {
            for t in d.allowed_tools {
                assert!(base.get(t).is_some(), "domain {} references unknown tool {t}", d.tool_name);
            }
        }
    }

    #[test]
    fn all_specialist_whitelists_resolve() {
        let base = ToolRegistry::build_v1();
        for s in SPECIALISTS {
            for t in s.allowed_tools {
                assert!(base.get(t).is_some(), "specialist {} references unknown tool {t}", s.tool_name);
            }
        }
    }

    #[test]
    fn every_specialist_has_a_matching_parent_domain() {
        for s in SPECIALISTS {
            assert!(
                DOMAINS.iter().any(|d| d.tool_name == s.parent_domain),
                "specialist {} has orphan parent_domain {}",
                s.tool_name,
                s.parent_domain
            );
        }
    }

    #[test]
    fn delegate_tool_names_are_globally_unique() {
        // Duplicate names would collide in a sub-registry's HashMap, silently
        // shadowing one specialist with another.
        let mut seen = std::collections::HashSet::new();
        for name in DOMAINS.iter().map(|d| d.tool_name).chain(SPECIALISTS.iter().map(|s| s.tool_name)) {
            assert!(seen.insert(name), "duplicate delegate tool name: {name}");
        }
    }
}
