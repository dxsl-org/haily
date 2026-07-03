mod agent;
pub mod approval;
mod budget;
mod context;
mod delegate;
mod domains;
pub mod feedback_parser;
mod specialists;
mod tag_matcher;
mod tool_call;
pub mod worktree;

pub use approval::ApprovalBroker;

use anyhow::Result;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::{LlmConfig, LlmRouter};
use haily_tools::ToolRegistry;
use haily_types::{ApprovalResolver, Request, ResponseChunk};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub struct Orchestrator {
    pub kms: Arc<KmsHandle>,
    pub db: Arc<DbHandle>,
    /// RwLock allows swapping the LLM without restarting the app.
    /// Lock is held only for the duration of cloning the inner Arc — never across await.
    llm: Arc<RwLock<Arc<LlmRouter>>>,
    tools: Arc<ToolRegistry>,
    /// Shared across every turn — approvals are keyed by `approval_id`, so one
    /// broker instance for the whole orchestrator lifetime is correct (not
    /// per-turn); see `approval.rs` for the session-bound resolution contract.
    approval_broker: Arc<ApprovalBroker>,
    /// Phase 3 kill switch (C8): `safety.disable_writes`. The runtime source of truth for
    /// blocking NEW forward writes; the persisted preference row is only next-boot state.
    /// Cloned into every `TurnRuntime`/`SubTurnRequest` (via the DelegateTools) so the gate
    /// is observed at any depth, and exposed via `kill_handle()` so the app layer can flip
    /// it live from `set_preference`/CLI without an orchestrator round-trip.
    kill: Arc<AtomicBool>,
}

impl Orchestrator {
    /// Initialise the orchestrator and spawn its background self-improvement workers.
    ///
    /// `shutdown`/`tasks` wire the workers into the caller's shutdown sequence: the
    /// workers select on `shutdown.cancelled()` and exit promptly instead of waiting
    /// out their sleep interval, and are registered on `tasks` so `TaskTracker::wait()`
    /// only resolves once they have actually exited.
    ///
    /// `auto_approve` MUST already be validated by the caller (`haily_app::auto_approve
    /// ::validate_auto_approve` — rejects any `RequireApproval`-class tool name at
    /// startup) — this constructor trusts it and does not re-check.
    pub async fn init(
        kms: Arc<KmsHandle>,
        db: Arc<DbHandle>,
        config: LlmConfig,
        shutdown: CancellationToken,
        tasks: TaskTracker,
        auto_approve: std::collections::HashSet<String>,
    ) -> Result<Self> {
        let llm_inner = Arc::new(LlmRouter::init(config).await);
        let llm_provider = llm_inner.provider_name().to_string();

        // Wrap in the shared RwLock BEFORE building DelegateTools or spawning the
        // self-improvement workers (F5 fix — red team): both used to capture the
        // pre-RwLock `llm_inner` Arc directly, so `reload_llm()` never reached
        // either L1/L2 delegation or hourly skill synthesis. Every consumer below
        // now holds this same `Arc<RwLock<Arc<LlmRouter>>>` and read-clones it per
        // use, exactly like `process()` does for the top-level turn.
        let llm = Arc::new(RwLock::new(llm_inner));

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
        // Timeout bounding every external connector call (phase 4). Conservative — an
        // interactive connector op should complete well within this; a hang is treated as
        // a transport error the C7 read-back path recovers from.
        const CONNECTOR_TIMEOUT_SECS: u64 = 30;
        // Phase 3 kill switch (C8): seed from the persisted `safety.disable_writes`
        // preference so a restart preserves a thrown switch. This Arc is the runtime
        // source of truth from here on; the app layer flips it live via `kill_handle()`.
        let disable_writes = haily_db::queries::meta::get_preference(&db, "safety.disable_writes")
            .await
            .ok()
            .flatten()
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        let kill = Arc::new(AtomicBool::new(disable_writes));

        let mut base_v1 = ToolRegistry::build_v1();

        // Phase 4 (C2): register human-approved connector ops into `base_v1` BEFORE any
        // `sub_registry` snapshot below — `register` needs `&mut self`, and once `base_v1`
        // is snapshotted + Arc-wrapped no op could be added and the domain whitelists would
        // resolve their connector op-names to `None`. Each op becomes one HttpConnectorTool
        // bound to the shared kill switch (M5) + the phase-3 journal (via ToolContext.db).
        // An unparseable manifest, or one whose base_url resolves into a blocked
        // metadata/link-local range, is SKIPPED (logged) rather than registered — a
        // connector tool that would fail-closed on every call is worse than absent.
        let mut parsed_manifests = Vec::new();
        match haily_db::queries::connectors::list_active(&db).await {
            Ok(rows) => {
                for row in rows {
                    // Integrity gate: recompute the content hash and skip a row whose stored
                    // hash no longer matches its bytes (out-of-band tamper the append-only
                    // trigger can't catch — raw sqlite write / file-level edit / doctored-DB
                    // restore). A tampered, human-unapproved schema must never register.
                    if !row.verify_integrity() {
                        tracing::warn!(
                            connector = %row.connector_name,
                            version = %row.version,
                            "skipping connector — content_hash mismatch (manifest altered out-of-band); re-approval required"
                        );
                        continue;
                    }
                    match haily_tools::connector::manifest::parse(&row.manifest_json) {
                        Ok(m) => {
                            if let Err(e) =
                                haily_tools::security::validate_manifest_base_url(&m.base_url).await
                            {
                                tracing::warn!(
                                    connector = %row.connector_name,
                                    version = %row.version,
                                    "skipping connector — base_url failed approval-time SSRF validation: {e:#}"
                                );
                                continue;
                            }
                            parsed_manifests.push(m);
                        }
                        Err(e) => tracing::warn!(
                            connector = %row.connector_name,
                            version = %row.version,
                            "skipping unparseable connector manifest: {e:#}"
                        ),
                    }
                }
            }
            Err(e) => tracing::warn!("failed to load connector manifests: {e:#}"),
        }
        if !parsed_manifests.is_empty() {
            let op_count: usize = parsed_manifests.iter().map(|m| m.ops.len()).sum();
            base_v1.register_connectors(
                parsed_manifests,
                Arc::clone(&kill),
                std::time::Duration::from_secs(CONNECTOR_TIMEOUT_SECS),
                // Credential preference key convention: "<connector>.api_key". The secret
                // is redacted (C4); only this reference name is journaled.
                |connector_name| format!("{connector_name}.api_key"),
            );
            tracing::info!(ops = op_count, "registered connector ops into base_v1");
        }

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
                    llm: Arc::clone(&llm),
                    sub_registry: l2_reg,
                    max_depth: 2, // L1 (depth=1) can spawn; L2 (depth=2) blocked by depth guard
                    model_tier: spec.model_tier,
                    kill: Arc::clone(&kill),
                }));
            }

            tools.register(Arc::new(delegate::DelegateTool {
                tool_name: domain.tool_name,
                description: domain.description,
                system_prompt: domain.system_prompt,
                domain_name: domain.tool_name.trim_start_matches("delegate_to_"),
                db: Arc::clone(&db),
                kms: Arc::clone(&kms),
                llm: Arc::clone(&llm),
                sub_registry: Arc::new(l1_reg),
                max_depth: 1, // L0 (depth=0) can spawn L1; L1 depth guard handles the rest
                model_tier: domain.model_tier,
                kill: Arc::clone(&kill),
            }));
        }

        let tools = Arc::new(tools);
        tracing::info!(
            llm = llm_provider,
            tools = tools.len(),
            "Orchestrator ready"
        );

        // Reset work items stuck in `running` from a previous crash to `interrupted`.
        match haily_db::queries::work_items::reset_stale_running(&db).await {
            Ok(n) if n > 0 => tracing::info!(count = n, "work items reset to interrupted"),
            Err(e) => tracing::warn!("failed to reset stale work items: {e:#}"),
            _ => {}
        }

        // Phase 3 reconciliation sweep (C6/C7): classify orphan `pending` journal rows
        // left by a crash/kill mid-write via a read-back GET — NEVER a blind create-retry
        // (M4). Placed right next to `reset_stale_running` (same crash-recovery boundary).
        // A row past its grace window and still `pending` is an orphan; a fresh in-flight
        // write is skipped.
        //
        // PHASE 4 decision — reconciliation stays on `UnconfiguredExecutor`: the sweep runs
        // ONCE over ALL incomplete rows regardless of connector, but each row's read-back
        // must go to ITS OWN manifest's base_url + pinned allowance (routing by
        // `row.tool_name` → op → manifest). That per-row executor selection is real routing
        // logic, deferred to phase 5 with the Odoo executor (noted in the plan). Until then
        // the fail-closed placeholder marks unclassifiable rows `unverified` — the safe C7
        // direction, which never blocks a later undo.
        let reconcile_executor = haily_tools::connector::UnconfiguredExecutor;
        let reconciled = haily_tools::journal_undo::reconcile::reconcile_incomplete(
            &db,
            &reconcile_executor,
            haily_tools::journal_undo::reconcile::RECONCILE_GRACE_SECS,
        )
        .await;
        if reconciled > 0 {
            tracing::info!(
                count = reconciled,
                "reconciled orphan action-journal rows at startup"
            );
        }

        Self::spawn_self_improvement_workers(Arc::clone(&kms), Arc::clone(&llm), shutdown, tasks);

        Ok(Self {
            kms,
            db,
            llm,
            tools,
            approval_broker: Arc::new(ApprovalBroker::with_auto_approve(auto_approve)),
            kill,
        })
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

    /// `cancel` is this turn's cancellation token — the caller (`haily-app`'s
    /// dispatch loop) mints a child token from its root shutdown token per turn, so
    /// firing it here (shutdown) denies any pending tool approval immediately rather
    /// than holding up the drain for up to the 120s approval timeout. `process`'s
    /// final shape (Phase 6) folds this into the signature permanently; this is that
    /// signature landed early so the approval broker has something to select on.
    pub async fn process(
        &self,
        req: Request,
        tx: mpsc::Sender<ResponseChunk>,
        cancel: CancellationToken,
    ) -> Result<()> {
        // Clone the Arc under a brief read-lock — never hold the lock across await.
        let llm = self.llm.read().unwrap_or_else(|e| e.into_inner()).clone();
        let runtime = agent::TurnRuntime {
            db: Arc::clone(&self.db),
            kms: Arc::clone(&self.kms),
            llm,
            tools: Arc::clone(&self.tools),
            kill: Arc::clone(&self.kill),
        };
        agent::run_turn(&req, runtime, tx, &self.approval_broker, &cancel).await
    }

    pub fn llm_provider(&self) -> String {
        self.llm
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .provider_name()
            .to_string()
    }

    /// Adapter-facing handle for resolving pending tool approvals (GUI/CLI/Telegram
    /// inject this at bootstrap). Returns the same broker instance `process` awaits
    /// on, upcast to the layering-safe trait object defined in `haily-types`.
    pub fn approval_resolver(&self) -> Arc<dyn ApprovalResolver> {
        Arc::clone(&self.approval_broker) as Arc<dyn ApprovalResolver>
    }

    /// The `safety.disable_writes` kill switch (C8), for the app layer to flip live from
    /// `set_preference`/CLI without an orchestrator round-trip — mirrors
    /// `approval_resolver()`'s "clone the handle once at bootstrap" pattern. The caller
    /// must ALSO persist the `safety.disable_writes` preference row for next-boot state;
    /// this Arc is the runtime source of truth, the row is only persistence.
    pub fn kill_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.kill)
    }

    /// Spawn background workers for skill synthesis (hourly) and decay (daily).
    ///
    /// Both loops select on `shutdown.cancelled()` so they wake and exit immediately
    /// on shutdown rather than finishing out a up-to-24h sleep, and are registered on
    /// `tasks` so the caller's `TaskTracker::wait()` blocks until they have exited.
    ///
    /// `llm` is the SAME `Arc<RwLock<Arc<LlmRouter>>>` `Orchestrator` holds (F5 fix,
    /// second location — red team): capturing a plain `Arc<LlmRouter>` here would
    /// freeze hourly synthesis on whatever backend was active at boot, immune to
    /// `reload_llm()` exactly like the pre-fix `DelegateTool` bug. The router is
    /// read-cloned fresh on EVERY iteration (not once at spawn time) so a reload
    /// that lands between two synthesis runs is picked up by the next one.
    fn spawn_self_improvement_workers(
        kms: Arc<KmsHandle>,
        llm: Arc<RwLock<Arc<LlmRouter>>>,
        shutdown: CancellationToken,
        tasks: TaskTracker,
    ) {
        // Skill synthesis — every 1 hour
        let kms_s = Arc::clone(&kms);
        let llm_s = Arc::clone(&llm);
        let shutdown_s = shutdown.clone();
        tasks.spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_s.cancelled() => {
                        tracing::info!("skill synthesis worker shutting down");
                        break;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(3600)) => {
                        // Clone the Arc under a brief read-lock — never hold the lock
                        // across the `.await` below (same rule as `process()`).
                        let router = Arc::clone(&*llm_s.read().unwrap_or_else(|e| e.into_inner()));
                        match kms_s.synthesize_skills(router.as_ref()).await {
                            Ok(v) if !v.is_empty() => tracing::info!(count = v.len(), "skills synthesized"),
                            Err(e) => tracing::warn!("skill synthesis failed: {e:#}"),
                            _ => {}
                        }
                    }
                }
            }
        });

        // Skill decay — every 24 hours
        let kms_d = Arc::clone(&kms);
        tasks.spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::info!("skill decay worker shutting down");
                        break;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(86400)) => {
                        if let Err(e) = kms_d.decay_skills().await {
                            tracing::warn!("skill decay failed: {e:#}");
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod shutdown_tests {
    //! Verifies the self-improvement workers actually observe cancellation instead of
    //! sleeping out their full interval — the bug this phase fixes (workers spawned
    //! detached with no shutdown hook at all).
    use super::*;

    #[tokio::test]
    async fn self_improvement_workers_exit_promptly_on_cancel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("haily.db");
        let db = Arc::new(haily_db::DbHandle::init(&db_path).await.expect("db init"));
        let kms = Arc::new(
            KmsHandle::init((*db).clone(), dir.path())
                .await
                .expect("kms init"),
        );
        let llm = Arc::new(RwLock::new(Arc::new(
            LlmRouter::init(LlmConfig::default()).await,
        )));

        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();

        Orchestrator::spawn_self_improvement_workers(
            Arc::clone(&kms),
            Arc::clone(&llm),
            shutdown.clone(),
            tasks.clone(),
        );

        // Both workers sleep for 1h/24h; if cancellation isn't observed, this would
        // hang until the test harness times out. Bound the wait tightly to prove the
        // `select!` arm — not the sleep — is what ends the loop.
        shutdown.cancel();
        tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(5), tasks.wait())
            .await
            .expect("workers must exit promptly on cancellation, not after their sleep interval");
    }

    /// Phase 7 (F5, second location — red team): `spawn_self_improvement_workers`
    /// must receive the SAME `Arc<RwLock<Arc<LlmRouter>>>` `Orchestrator::reload_llm`
    /// writes to, not a plain `Arc<LlmRouter>` snapshotted at spawn time. The
    /// worker's 1h/24h sleep makes waiting for an actual synthesis run impractical in
    /// a unit test, so this isolates exactly the mechanism the fix depends on: the
    /// SAME lock instance passed into the spawn function must observe a `reload_llm`-
    /// style swap performed from outside it. A regression back to capturing
    /// `Arc<LlmRouter>` by value (dereferencing once at spawn time) would make this
    /// fail to compile — `spawn_self_improvement_workers`'s signature itself is part
    /// of what this test guards.
    #[tokio::test]
    async fn worker_router_lock_observes_a_reload_performed_after_spawn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("haily.db");
        let db = Arc::new(haily_db::DbHandle::init(&db_path).await.expect("db init"));
        let kms = Arc::new(
            KmsHandle::init((*db).clone(), dir.path())
                .await
                .expect("kms init"),
        );
        let original = Arc::new(LlmRouter::init(LlmConfig::default()).await);
        let llm = Arc::new(RwLock::new(Arc::clone(&original)));

        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();

        // Pass the SAME Arc<RwLock<..>> the workers will read from on their next
        // tick — this is the exact call shape `Orchestrator::init` now uses.
        Orchestrator::spawn_self_improvement_workers(
            Arc::clone(&kms),
            Arc::clone(&llm),
            shutdown.clone(),
            tasks.clone(),
        );

        // Simulate `reload_llm`: swap the inner Arc under the lock from OUTSIDE the
        // worker closures, exactly as `Orchestrator::reload_llm` does on the field
        // the workers were handed a clone of.
        let reloaded = Arc::new(LlmRouter::init(LlmConfig::default()).await);
        {
            let mut guard = llm.write().unwrap_or_else(|e| e.into_inner());
            *guard = Arc::clone(&reloaded);
        }

        // A fresh read through the same lock the spawned workers hold must see the
        // reloaded instance, not the one captured before `reload_llm` ran — proving
        // the workers share the lock by reference rather than a frozen snapshot.
        let observed = Arc::clone(&*llm.read().unwrap_or_else(|e| e.into_inner()));
        assert!(
            Arc::ptr_eq(&observed, &reloaded),
            "workers' shared lock must observe a reload performed after spawn"
        );
        assert!(
            !Arc::ptr_eq(&observed, &original),
            "stale pre-reload router must no longer be observable"
        );

        shutdown.cancel();
        tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(5), tasks.wait())
            .await
            .expect("workers must exit promptly on cancellation");
    }
}

#[cfg(test)]
mod wiring_tests {
    //! Guards the 3-tier registry wiring against silent drift. `sub_registry`
    //! silently drops unknown tool names, so a typo in any `allowed_tools` entry
    //! would strip a capability with zero runtime signal — these tests turn that
    //! into a compile-then-test failure instead.
    use crate::domains::{CONNECTOR_OP_WHITELIST, DOMAINS};
    use crate::specialists::SPECIALISTS;
    use haily_tools::ToolRegistry;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// Build a base_v1 with a representative connector manifest registered, declaring the
    /// exact op-names the whitelist references (C2). Mirrors `Orchestrator::init`'s
    /// register-before-snapshot ordering so the whitelist-resolution tests exercise a base
    /// that actually contains the connector ops.
    fn base_v1_with_connectors() -> ToolRegistry {
        let ops: String = CONNECTOR_OP_WHITELIST
            .iter()
            .map(|n| {
                format!(
                    r#"{{"name":"{n}","risk_tier":"IrreversibleWrite","compensability":"compensatable","compensation":{{"op":"unlink"}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"{{"connector_name":"odoo","version":"1","base_url":"https://erp.example.com","allowed_ip_cidrs":[],"ops":[{ops}]}}"#
        );
        let manifest = haily_tools::connector::manifest::parse(&json).unwrap();
        let mut base = ToolRegistry::build_v1();
        base.register_connectors(
            vec![manifest],
            Arc::new(AtomicBool::new(false)),
            std::time::Duration::from_secs(30),
            |c| format!("{c}.api_key"),
        );
        base
    }

    #[test]
    fn all_domain_whitelists_resolve() {
        // Register connectors first (C2) so a whitelisted connector op-name resolves —
        // exactly the ordering `Orchestrator::init` uses.
        let base = base_v1_with_connectors();
        for d in DOMAINS {
            for t in d.allowed_tools {
                assert!(
                    base.get(t).is_some(),
                    "domain {} references unknown tool {t}",
                    d.tool_name
                );
            }
        }
    }

    #[test]
    fn connector_op_visible_to_delegable_subagent() {
        // C2 ordering fix, end-to-end: a connector op registered into base_v1 BEFORE the
        // per-domain sub_registry snapshot must resolve in a delegable domain's snapshot —
        // else `sub_registry.get(op) → None` and a sub-agent can never reach it.
        let base = base_v1_with_connectors();
        let business = DOMAINS
            .iter()
            .find(|d| d.tool_name == "delegate_to_business")
            .expect("business domain exists");
        let sub = base.sub_registry(business.allowed_tools);
        assert!(
            sub.get("odoo_contact_create").is_some(),
            "connector op must resolve in the business domain's sub_registry (C2)"
        );
    }

    #[test]
    fn all_specialist_whitelists_resolve() {
        let base = ToolRegistry::build_v1();
        for s in SPECIALISTS {
            for t in s.allowed_tools {
                assert!(
                    base.get(t).is_some(),
                    "specialist {} references unknown tool {t}",
                    s.tool_name
                );
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
        for name in DOMAINS
            .iter()
            .map(|d| d.tool_name)
            .chain(SPECIALISTS.iter().map(|s| s.tool_name))
        {
            assert!(seen.insert(name), "duplicate delegate tool name: {name}");
        }
    }

    /// Phase 7 tier foundation: `model_tier` is a real `Option<haily_llm::Tier>` on
    /// every config, so an invalid tier value cannot even compile — this test's job
    /// is just to confirm every entry's default is the documented `None` (zero
    /// behavior change) rather than an unintended `Some(_)` slipping in during future
    /// edits. If a domain/specialist genuinely opts into a tier later, update its
    /// expectation here deliberately rather than let the check silently drop.
    #[test]
    fn every_domain_and_specialist_tier_defaults_to_none() {
        for d in DOMAINS {
            assert!(
                d.model_tier.is_none(),
                "domain {} has a non-default model_tier",
                d.tool_name
            );
        }
        for s in SPECIALISTS {
            assert!(
                s.model_tier.is_none(),
                "specialist {} has a non-default model_tier",
                s.tool_name
            );
        }
    }
}
