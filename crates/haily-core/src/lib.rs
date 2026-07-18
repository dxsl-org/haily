mod agent;
pub mod approval;
mod budget;
/// Chat-intent classifier for pipeline auto-detection (Pipeline Activation & Wiring, phase 2).
/// Lives here (not `haily-app`) because it reuses `feedback_parser`'s `pub(crate)` anchor
/// primitives — see that module's doc comment.
pub mod coding_intent;
mod context;
pub mod depth;
mod delegate;
mod domains;
pub mod feedback_parser;
pub mod pipeline;
pub mod routing;
mod specialists;
mod tag_matcher;
mod tier_intent;
mod tool_call;
/// View Engine Phase A — in-memory `ViewStore` for `DataView` snapshots. Public because
/// Phase 3 wires `Arc<ViewStore>` into the Orchestrator and beyond; not yet bootstrap-
/// threaded in this phase (see `view::store` doc comment).
pub mod view;
pub mod worktree;

pub use approval::{ApprovalBroker, PendingApproval};
/// Pipeline Activation & Wiring (phase 2): the chat-intent classifier's public entrypoint,
/// re-exported at the crate root so `haily-app::trigger` never needs the `coding_intent` path.
pub use coding_intent::classify as classify_coding_intent;
/// Pipeline Activation & Wiring (phase 1): the caller-facing launch request type + its Plan/
/// Build/PlanThenBuild selector, re-exported at the crate root so an app-layer caller (`haily-
/// app`) never needs a `haily_core::pipeline::launcher` path.
pub use pipeline::{CodingRunSpec, RunKind};

use anyhow::Result;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::{LlmConfig, LlmRouter};
use haily_tools::ToolRegistry;
use haily_types::{ApprovalGate, ApprovalResolver, Notification, Request, ResponseChunk, RunEvent};
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
    /// Auto Model Routing R1 (phase 4) kill switch: `llm.routing_enabled`. Mirrors `kill`
    /// exactly — seeded once from the persisted preference (default ON), the runtime
    /// source of truth from here on, and exposed via `routing_enabled_handle()` so the app
    /// layer can flip it live from `set_preference` without an orchestrator round-trip.
    /// `false` collapses every turn's tier decision to `None` (identical model selection
    /// to a pre-phase-4 turn) and skips the `routing_decisions` telemetry write entirely.
    routing_enabled: Arc<AtomicBool>,
    /// View Engine Phase A (phase 3): the ONE `ViewStore` instance for this orchestrator's
    /// lifetime — cloned into every turn/sub-turn `ToolContext` (as `Arc<dyn ViewSink>`, the
    /// write side) and exposed via [`Self::view_store`] (the read side, GUI-consumption-only;
    /// see the quarantine test in this module) so a tool's `present_view` insert and the
    /// `get_view` Tauri command observe the SAME snapshot. Mirrors `approval_broker`: one
    /// shared instance for the whole orchestrator lifetime, never per-turn.
    view_store: Arc<view::ViewStore>,
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
    ///
    /// `credential_getter` (Safe Operator Harness phase 2) is forwarded to
    /// `register_connectors` → every `HttpExecutor`, so a manifest's declared `auth` section
    /// can be resolved at call time. `None` is a legitimate value (no connector manifest
    /// declares `auth` yet, or the app layer opted out) — it only matters for manifests that
    /// actually declare `auth`, which then fail closed rather than sending an unauthenticated
    /// request (see `HttpExecutor::resolve_auth`).
    pub async fn init(
        kms: Arc<KmsHandle>,
        db: Arc<DbHandle>,
        config: LlmConfig,
        shutdown: CancellationToken,
        tasks: TaskTracker,
        auto_approve: std::collections::HashSet<String>,
        credential_getter: Option<Arc<dyn haily_tools::connector::CredentialGetter>>,
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
            // Authored-skill lazy-load (phase 2) — universal, like Claude Code's Read/Skill.
            "skill_search",
            "skill_list_sections",
            "skill_fetch",
        ];
        // Timeout bounding every external connector call (phase 4). Conservative — an
        // interactive connector op should complete well within this; a hang is treated as
        // a transport error the C7 read-back path recovers from.
        const CONNECTOR_TIMEOUT_SECS: u64 = 30;
        // C3 (Activate-and-Measure phase 4b): outer bound on the ENTIRE startup
        // reconciliation sweep, regardless of row count or how many connector hosts turn
        // out to be unreachable. The sweep's own per-executor short-circuit (reconcile.rs)
        // already stops hammering a dead host after its first failure; this is a second,
        // coarser belt-and-suspenders bound on the background task as a whole.
        const RECONCILE_SWEEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
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

        // Auto Model Routing R1 (phase 4) kill switch: mirrors `disable_writes` above
        // exactly — seeded from the persisted `llm.routing_enabled` preference, default
        // ON (routing_enabled=false is the escape hatch, not the shipping default: a
        // strict superset of legacy behavior is safe to enable by default). Live-flipped
        // from here on via `routing_enabled_handle()` (see that method's doc).
        let routing_enabled_pref = haily_db::queries::meta::get_preference(&db, "llm.routing_enabled")
            .await
            .ok()
            .flatten()
            .map(|v| v == "true" || v == "1")
            .unwrap_or(true);
        let routing_enabled = Arc::new(AtomicBool::new(routing_enabled_pref));

        let mut base_v1 = ToolRegistry::build_v1();

        // Phase 4 (C2): register human-approved connector ops into `base_v1` BEFORE any
        // `sub_registry` snapshot below — `register` needs `&mut self`, and once `base_v1`
        // is snapshotted + Arc-wrapped no op could be added and the domain whitelists would
        // resolve their connector op-names to `None`. Each op becomes one HttpConnectorTool
        // bound to the shared kill switch (M5) + the phase-3 journal (via ToolContext.db).
        // An unparseable manifest, or one whose base_url resolves into a blocked
        // metadata/link-local range, is SKIPPED (logged) rather than registered — a
        // connector tool that would fail-closed on every call is worse than absent.
        // M2 (Activate-and-Measure phase 4b): each manifest is paired with its OWN
        // `content_hash` (already integrity-verified above) — pinned into every journal
        // row `register_connectors` wires up, and into the `ConnectorResolver` the undo/
        // reconcile paths compare a row's pinned hash against.
        let mut parsed_manifests: Vec<(haily_tools::connector::Manifest, String)> = Vec::new();
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
                            parsed_manifests.push((m, row.content_hash.clone()));
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
        // M5c: the op→executor(+manifest-hash) routing table `register_connectors` builds —
        // handed to the background reconcile sweep below. Empty when no manifest registered
        // (every op then fails closed to "unconfigured" in undo/reconcile, matching the prior
        // placeholder's intent).
        let connector_routing = if !parsed_manifests.is_empty() {
            let op_count: usize = parsed_manifests.iter().map(|(m, _)| m.ops.len()).sum();
            let routing = base_v1.register_connectors(
                parsed_manifests,
                Arc::clone(&kill),
                std::time::Duration::from_secs(CONNECTOR_TIMEOUT_SECS),
                // Credential preference key convention: "<connector>.api_key". The secret
                // is redacted (C4); only this reference name is journaled.
                |connector_name| format!("{connector_name}.api_key"),
                credential_getter.clone(),
            );
            tracing::info!(ops = op_count, "registered connector ops into base_v1");
            routing
        } else {
            haily_tools::journal_undo::ConnectorResolver::new()
        };

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
                // Phase 2: every specialist gets the universal skill lazy-load trio on
                // top of its narrow whitelist — injected here (not copied into 14
                // literals) since several specialists share identical tool lists.
                let mut l2_inner = base_v1.sub_registry(spec.allowed_tools);
                for name in domains::SKILL_TOOLS {
                    if let Some(t) = base_v1.get(name) {
                        l2_inner.register(Arc::clone(t));
                    }
                }
                let l2_reg = Arc::new(l2_inner);
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

        // M8a (Activate-and-Measure phase 4b): WARN for any domain/specialist tool
        // reference with no registered implementation — most commonly a connector op a
        // manifest never registered (Odoo not configured, or a manifest revoked) but a
        // domain still whitelists it by name. `sub_registry` silently drops an unresolved
        // name (C2, by design), so without this WARN a "CRM" domain feature would be
        // dormant with zero operator-visible signal. Landed here (not phase 5) because this
        // is exactly the point `base_v1` reflects every op that DID register.
        for domain in domains::DOMAINS {
            for name in domain.allowed_tools {
                if base_v1.get(name).is_none() {
                    tracing::warn!(
                        domain = domain.tool_name,
                        tool = *name,
                        "domain whitelists a tool with no registered implementation \
                         (connector not configured, manifest revoked, or a stale name)"
                    );
                }
            }
        }
        for spec in specialists::SPECIALISTS {
            for name in spec.allowed_tools {
                if base_v1.get(name).is_none() {
                    tracing::warn!(
                        specialist = spec.tool_name,
                        tool = *name,
                        "specialist whitelists a tool with no registered implementation"
                    );
                }
            }
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

        // Sub-Agent + Skill Architecture phase 4b — pipeline resume reconcile (FMA-C1/m4): a
        // pipeline run left `running`/`queued` by a crash/kill is reset to `interrupted` so it
        // surfaces for EXPLICIT user resume and never auto-resumes a write stage. Mirrors the
        // work-item reset above; the persisted `attempts_remaining` bound already survived the
        // restart, so a resumed run cannot exceed its liveness budget.
        match haily_db::queries::pipeline_runs::reset_stale_running(&db).await {
            Ok(n) if n > 0 => tracing::info!(count = n, "pipeline runs reset to interrupted"),
            Err(e) => tracing::warn!("failed to reset stale pipeline runs: {e:#}"),
            _ => {}
        }

        // Phase 3 reconciliation sweep (C6/C7): classify orphan `pending` journal rows
        // left by a crash/kill mid-write via a read-back GET — NEVER a blind create-retry
        // (M4). A row past its grace window and still `pending` is an orphan; a fresh
        // in-flight write is skipped.
        //
        // C3 (Activate-and-Measure phase 4b): the sweep now routes each row to its OWN
        // manifest's executor via `connector_routing` (M5c) instead of the phase-3/4
        // fail-closed placeholder — a real read-back is a serial, `CONNECTOR_TIMEOUT_SECS`-
        // bounded network call (reconcile.rs), so awaiting the WHOLE sweep here would hang
        // boot for up to `N * CONNECTOR_TIMEOUT_SECS` with a single unreachable connector
        // host. Spawn it as a background task instead: it selects on shutdown, is bounded
        // by `RECONCILE_SWEEP_TIMEOUT` for the sweep as a whole, and `reconcile_incomplete`
        // itself short-circuits per-executor after its first read-back failure (never
        // retry-storms a dead host). Boot returns without awaiting any of this — an orphan
        // row simply stays `pending` a little longer, which is safe (see the phase's Risk
        // Notes: the fail-closed direction never blocks a later undo).
        let reconcile_db = Arc::clone(&db);
        let reconcile_shutdown = shutdown.child_token();
        tasks.spawn(async move {
            tokio::select! {
                _ = reconcile_shutdown.cancelled() => {
                    tracing::info!("startup reconciliation sweep cancelled by shutdown");
                }
                result = tokio::time::timeout(
                    RECONCILE_SWEEP_TIMEOUT,
                    haily_tools::journal_undo::reconcile::reconcile_incomplete(
                        &reconcile_db,
                        &connector_routing,
                        haily_tools::journal_undo::reconcile::RECONCILE_GRACE_SECS,
                    ),
                ) => {
                    match result {
                        Ok(n) if n > 0 => {
                            tracing::info!(count = n, "reconciled orphan action-journal rows at startup");
                        }
                        Ok(_) => {}
                        Err(_) => tracing::warn!(
                            timeout_secs = RECONCILE_SWEEP_TIMEOUT.as_secs(),
                            "startup reconciliation sweep timed out — remaining orphan rows stay pending"
                        ),
                    }
                }
            }
        });

        Self::spawn_self_improvement_workers(Arc::clone(&kms), Arc::clone(&llm), shutdown, tasks);

        Ok(Self {
            kms,
            db,
            llm,
            tools,
            approval_broker: Arc::new(ApprovalBroker::with_auto_approve(auto_approve)),
            kill,
            routing_enabled,
            view_store: Arc::new(view::ViewStore::new()),
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
            routing_enabled: Arc::clone(&self.routing_enabled),
            view_store: Arc::clone(&self.view_store),
        };
        agent::run_turn(&req, runtime, tx, &self.approval_broker, &cancel).await
    }

    /// Launch a live coding-pipeline run (Pipeline Activation & Wiring, phase 1) bound to this
    /// orchestrator's own handles. `LaunchDeps` is built here (never inside `pipeline::launcher`,
    /// which does not know about `Orchestrator`'s private fields — mirrors `process`'s own
    /// `TurnRuntime` construction) from the SAME approval broker + kill switch a normal turn
    /// uses, so every write-tier tool call inside the launched run stays gated identically
    /// (Security Considerations). `cancel` is the caller's per-run token, mirroring `process`'s
    /// per-turn token.
    ///
    /// # Errors
    /// Returns an error only for a setup failure (target-repo resolution, workspace open, or a
    /// runner setup failure) — a paused/failed pipeline run is a normal `RunReport`, not an
    /// error (see `pipeline::launcher::launch_coding_run`'s contract).
    pub async fn launch_coding_run(
        &self,
        spec: pipeline::CodingRunSpec,
        user_tx: mpsc::Sender<ResponseChunk>,
        events_tx: mpsc::Sender<RunEvent>,
        distillation_tx: Option<mpsc::Sender<Notification>>,
        cancel: CancellationToken,
    ) -> Result<pipeline::RunReport> {
        let deps = pipeline::LaunchDeps {
            db: Arc::clone(&self.db),
            kms: Arc::clone(&self.kms),
            llm: Arc::clone(&self.llm),
            broker: Arc::clone(&self.approval_broker) as Arc<dyn ApprovalGate>,
            kill: Arc::clone(&self.kill),
        };
        pipeline::launch_coding_run(deps, spec, user_tx, events_tx, distillation_tx, cancel).await
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

    /// View Engine Phase A (phase 3): the shared `ViewStore` handle, read via
    /// `app_handle.orchestrator.view_store()` — exactly how `approve_tool` reaches
    /// `app_handle.orchestrator.approval_resolver()`. GUI-CONSUMPTION-ONLY (SEC F2): this is
    /// the read side (`get`); no `haily-core`/`haily-tools` code may call it (see the
    /// `view_store_quarantine_tests` module below) — the write side is `ToolContext.view_sink`
    /// (`Arc<dyn ViewSink>`, insert-only).
    pub fn view_store(&self) -> Arc<view::ViewStore> {
        Arc::clone(&self.view_store)
    }

    /// App-layer-facing handle for RAISING a new approval request (not resolving one) —
    /// Pipeline Activation & Wiring phase 2's confirm-gated chat-intent launch needs to request
    /// its OWN pending approval (a run-launch confirmation) through the exact same broker a
    /// normal turn's tool-approval flow uses (`process` → `agent::run_turn` →
    /// `tool_call::dispatch`), so the confirm prompt shares one unified pending-approvals queue
    /// with every other in-flight tool approval rather than a parallel policy. Mirrors
    /// `approval_resolver()`'s "clone the handle once, upcast to the layering-safe trait object"
    /// pattern, on the request-side trait instead of the resolve-side one.
    pub fn approval_gate(&self) -> Arc<dyn ApprovalGate> {
        Arc::clone(&self.approval_broker) as Arc<dyn ApprovalGate>
    }

    /// The `safety.disable_writes` kill switch (C8), for the app layer to flip live from
    /// `set_preference`/CLI without an orchestrator round-trip — mirrors
    /// `approval_resolver()`'s "clone the handle once at bootstrap" pattern. The caller
    /// must ALSO persist the `safety.disable_writes` preference row for next-boot state;
    /// this Arc is the runtime source of truth, the row is only persistence.
    pub fn kill_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.kill)
    }

    /// Auto Model Routing R1 (phase 4): the `llm.routing_enabled` kill switch, for the app
    /// layer to flip live from `set_preference` without an orchestrator round-trip — mirrors
    /// `kill_handle()` exactly (same rationale: `set_preference` sits behind the
    /// shutdown-guarded `app` Mutex and has no orchestrator access of its own).
    pub fn routing_enabled_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.routing_enabled)
    }

    /// Snapshot of every in-flight tool approval across all channels (phase 11a), for the
    /// unified approvals queue. Reconcile source only — the descriptive tool payload lives
    /// in the `ToolApprovalRequest` chunk the origin channel received; each entry's
    /// `session_id` is the auth boundary for resolving it.
    pub fn pending_approvals(&self) -> Vec<approval::PendingApproval> {
        self.approval_broker.pending_snapshot()
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
    use crate::domains::{CONNECTOR_OP_WHITELIST, DOMAINS, SCOUT_CODING_TOOLS};
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
            vec![(manifest, "test-hash".to_string())],
            Arc::new(AtomicBool::new(false)),
            std::time::Duration::from_secs(30),
            |c| format!("{c}.api_key"),
            None,
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
                // Browser tools (Phase 13) are registered only under the `browser` cargo feature
                // (default OFF); like connector ops before a manifest exists, their whitelist
                // names are inert-but-listed and `sub_registry` skips them when unregistered. In
                // the default test build they will not resolve, so skip them here — a dedicated
                // `#[cfg(feature = "browser")]` test in haily-tools asserts they register.
                if haily_tools::browser::BROWSER_TOOL_NAMES.contains(t) {
                    continue;
                }
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
    fn scout_coding_tools_resolve() {
        // The read-only coding subset (P5 scout stage) must resolve in build_v1 so wiring it
        // to a future scout sub_registry cannot silently drop a capability.
        let base = ToolRegistry::build_v1();
        for t in SCOUT_CODING_TOOLS {
            assert!(base.get(t).is_some(), "scout coding tool {t} missing from build_v1");
        }
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
    fn skill_tools_resolve_and_reach_every_domain() {
        // Phase 2: the universal skill lazy-load trio must exist in build_v1 and appear
        // in every domain's whitelisted sub_registry (domains list them explicitly).
        use crate::domains::SKILL_TOOLS;
        let base = ToolRegistry::build_v1();
        for t in SKILL_TOOLS {
            assert!(base.get(t).is_some(), "skill tool {t} missing from build_v1");
        }
        for d in DOMAINS {
            let sub = base.sub_registry(d.allowed_tools);
            for t in SKILL_TOOLS {
                assert!(
                    sub.get(t).is_some(),
                    "domain {} sub_registry is missing skill tool {t}",
                    d.tool_name
                );
            }
        }
    }

    /// Phase 7 (apex judge, LOCKED): the `judge` specialist's whitelist is READ-ONLY by
    /// construction. Its sub-registry must resolve `fs_read`/`fs_grep` and must resolve NO
    /// write/exec/delegate tool — a judge that cannot write cannot drift into "fixing things",
    /// which is the inherited hard rule the cost model depends on. `sub_registry` drops any
    /// name not in the whitelist, so an attempted write tool resolving to `None` IS the proof.
    #[test]
    fn judge_specialist_whitelist_is_read_only() {
        let base = ToolRegistry::build_v1();
        let judge = SPECIALISTS
            .iter()
            .find(|s| s.tool_name == "delegate_to_judge")
            .expect("judge specialist exists");
        let sub = base.sub_registry(judge.allowed_tools);
        for read_tool in ["fs_read", "fs_grep"] {
            assert!(sub.get(read_tool).is_some(), "judge must resolve read tool {read_tool}");
        }
        for write_tool in [
            "fs_write", "fs_edit", "fs_move", "fs_delete", "shell_exec", "code_exec", "git_commit",
        ] {
            assert!(
                sub.get(write_tool).is_none(),
                "judge is read-only — a write/exec tool ({write_tool}) must resolve to nothing"
            );
        }
        // Read-only also means it whitelists no delegation tool (never spawns work).
        assert!(
            !judge.allowed_tools.iter().any(|t| t.starts_with("delegate_to")),
            "judge must not be able to delegate"
        );
    }

    /// View Engine Phase A, phase 2 (quarantine): `present_view` must never appear in a
    /// sub-agent decision-surface whitelist — every `DOMAINS`/`SPECIALISTS` `allowed_tools`
    /// entry is a tool a delegated (depth >= 1) turn can reach, and an `LlmProjected` view is
    /// explicitly quarantined from ever being minted inside such a chain (belt-and-suspenders
    /// on top of `present_view`'s own `ctx.depth != 0` refusal and the sub-turn forwarder that
    /// already discards non-approval chunks — see `delegate.rs`). `present_view` is registered
    /// in `build_v1` for the interactive tool set only and is deliberately absent from every
    /// whitelist literal below; this test pins that absence so a future edit cannot silently
    /// add it to a delegation surface.
    #[test]
    fn present_view_excluded_from_every_delegation_whitelist() {
        for d in DOMAINS {
            assert!(
                !d.allowed_tools.contains(&"present_view"),
                "domain {} must not whitelist present_view (LlmProjected views are depth-0-only)",
                d.tool_name
            );
        }
        for s in SPECIALISTS {
            assert!(
                !s.allowed_tools.contains(&"present_view"),
                "specialist {} must not whitelist present_view (LlmProjected views are depth-0-only)",
                s.tool_name
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

#[cfg(test)]
mod routing_enabled_tests {
    //! Auto Model Routing R1 phase 4: proves the two halves `agent::turn`'s own
    //! `routing_toggle_tests` cannot — (1) `Orchestrator::init` seeds `routing_enabled`
    //! from the persisted `llm.routing_enabled` preference (mirrors `disable_writes`'s
    //! own boot-read exactly), and (2) `routing_enabled_handle()` is the SAME live Atomic
    //! `process()` reads on every turn, so flipping it takes effect on the very next
    //! turn with no restart — the "flipping the GUI toggle takes effect NEXT turn"
    //! success criterion, end-to-end through the real public `Orchestrator::process` path.
    use super::*;
    use haily_db::queries::{meta, routing_decisions};
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_plain_answer_sse_server(answer: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = stream.read(&mut buf).await;
                    let delta = serde_json::json!({
                        "choices": [{ "delta": { "content": answer } }]
                    })
                    .to_string();
                    let sse_body = format!("data: {delta}\n\ndata: [DONE]\n\n");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse_body}"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn cloud_config(base_url: String) -> LlmConfig {
        LlmConfig {
            cloud_api_keys: vec!["test-key".to_string()],
            cloud_base_url: base_url,
            cloud_model: "test-model".to_string(),
            ..LlmConfig::default()
        }
    }

    /// Returns the orchestrator plus its shutdown handles, so every test can drain its
    /// background workers cleanly at the end (mirrors `tests/fixtures/mod.rs`'s
    /// `run_golden_task` convention) instead of leaking them into the runtime's teardown.
    async fn init_orchestrator(
        db: Arc<DbHandle>,
        kms: Arc<KmsHandle>,
        base_url: String,
    ) -> (Orchestrator, CancellationToken, TaskTracker) {
        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();
        let orchestrator = Orchestrator::init(
            kms,
            db,
            cloud_config(base_url),
            shutdown.clone(),
            tasks.clone(),
            std::collections::HashSet::new(),
            None,
        )
        .await
        .expect("orchestrator init");
        (orchestrator, shutdown, tasks)
    }

    async fn cleanup(shutdown: CancellationToken, tasks: TaskTracker) {
        shutdown.cancel();
        tasks.close();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), tasks.wait()).await;
    }

    #[tokio::test]
    async fn routing_enabled_defaults_to_true_when_the_preference_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(DbHandle::init(&dir.path().join("haily.db")).await.expect("db init"));
        let kms = Arc::new(KmsHandle::init((*db).clone(), dir.path()).await.expect("kms init"));
        let base_url = spawn_plain_answer_sse_server("hi").await;

        let (orchestrator, shutdown, tasks) = init_orchestrator(db, kms, base_url).await;
        assert!(
            orchestrator.routing_enabled_handle().load(Ordering::Acquire),
            "absent llm.routing_enabled preference must default to ON"
        );
        cleanup(shutdown, tasks).await;
    }

    #[tokio::test]
    async fn routing_enabled_seeds_false_from_a_persisted_preference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(DbHandle::init(&dir.path().join("haily.db")).await.expect("db init"));
        meta::upsert_preference(&db, "llm.routing_enabled", "false", "test")
            .await
            .expect("seed preference");
        let kms = Arc::new(KmsHandle::init((*db).clone(), dir.path()).await.expect("kms init"));
        let base_url = spawn_plain_answer_sse_server("hi").await;

        let (orchestrator, shutdown, tasks) = init_orchestrator(db, kms, base_url).await;
        assert!(
            !orchestrator.routing_enabled_handle().load(Ordering::Acquire),
            "a persisted llm.routing_enabled=false preference must seed the Atomic OFF"
        );
        cleanup(shutdown, tasks).await;
    }

    /// HIGH: flipping `routing_enabled_handle()` between two `process()` calls on the
    /// SAME orchestrator changes the SECOND turn's behavior with no restart — proving
    /// the Atomic `process()` reads is the live source of truth, not a boot-time snapshot.
    #[tokio::test]
    async fn live_toggle_takes_effect_on_the_very_next_turn_without_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(DbHandle::init(&dir.path().join("haily.db")).await.expect("db init"));
        let kms = Arc::new(KmsHandle::init((*db).clone(), dir.path()).await.expect("kms init"));
        let base_url = spawn_plain_answer_sse_server("hello").await;
        let (orchestrator, shutdown, tasks) = init_orchestrator(Arc::clone(&db), kms, base_url).await;

        async fn drive_one_turn(orchestrator: &Orchestrator, message: &str) {
            let req = Request {
                session_id: uuid::Uuid::new_v4(),
                adapter_id: "test-adapter".to_string(),
                message: message.to_string(),
                user_ref: None,
                depth: Default::default(),
                origin: Default::default(),
            };
            let (tx, mut rx) = mpsc::channel(64);
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
            orchestrator
                .process(req, tx, CancellationToken::new())
                .await
                .expect("process");
            drain.await.expect("drain task");
        }

        // Turn 1: default ON — must write one routing_decisions row.
        drive_one_turn(&orchestrator, "hi there").await;
        assert_eq!(
            routing_decisions::list_recent(&db, 10).await.expect("list_recent").len(),
            1,
            "the first (routing-enabled) turn must write exactly one telemetry row"
        );

        // Flip live — no restart, no new Orchestrator.
        orchestrator.routing_enabled_handle().store(false, Ordering::Release);

        // Turn 2: same orchestrator instance — must write ZERO additional rows.
        drive_one_turn(&orchestrator, "what time is it").await;
        assert_eq!(
            routing_decisions::list_recent(&db, 10).await.expect("list_recent").len(),
            1,
            "flipping routing_enabled OFF must take effect on the VERY NEXT turn — no \
             new row from the second turn, with no restart of the orchestrator"
        );
        cleanup(shutdown, tasks).await;
    }
}

#[cfg(test)]
mod view_store_sharing_tests {
    //! View Engine Phase A (phase 3): proves `Orchestrator::view_store()` is a stable handle
    //! onto the ONE shared `ViewStore` — not a fresh instance minted per call — which is the
    //! property `get_view` (reading via a later `view_store()` call) depends on to ever see
    //! what a tool wrote (via the `Arc<dyn ViewSink>` a real turn's `ToolContext.view_sink`
    //! holds, itself `Arc::clone`d from this same field in `process()` — see `TurnRuntime`
    //! construction above). Simulates the write side through the `ViewSink` trait object
    //! exactly as a real tool call would, rather than depending on Phase 2's `present_view`
    //! tool (built concurrently) to exercise the path end-to-end.
    use super::*;
    use haily_types::{DataView, ProjectionKind, ProjectionSpec, ViewProvenance, ViewSink};

    fn sample_view() -> DataView {
        DataView {
            view_id: uuid::Uuid::new_v4(),
            entity: "contact".to_string(),
            schema: vec![],
            records: vec![],
            projections: vec![ProjectionSpec { kind: ProjectionKind::Table, binding: None }],
            active: ProjectionSpec { kind: ProjectionKind::Table, binding: None },
            total: None,
            cursor: None,
            provenance: ViewProvenance::LlmProjected,
        }
    }

    #[tokio::test]
    async fn tool_side_insert_and_command_side_get_observe_the_same_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(haily_db::DbHandle::init(&dir.path().join("haily.db")).await.expect("db init"));
        let kms = Arc::new(KmsHandle::init((*db).clone(), dir.path()).await.expect("kms init"));
        let shutdown = CancellationToken::new();
        let tasks = TaskTracker::new();
        let orchestrator = Orchestrator::init(
            kms,
            db,
            LlmConfig::default(),
            shutdown.clone(),
            tasks.clone(),
            std::collections::HashSet::new(),
            None,
        )
        .await
        .expect("orchestrator init");

        // Write side: exactly how a real `ToolContext.view_sink` (an `Arc<dyn ViewSink>`
        // cloned from `orchestrator.view_store()` via `TurnRuntime`) would insert.
        let write_handle: Arc<dyn ViewSink> = orchestrator.view_store();
        let view = sample_view();
        let view_id = view.view_id;
        write_handle.insert(view.clone());

        // Read side: a SEPARATE `view_store()` call — exactly how `get_view` reaches it from
        // the Tauri command layer. Must see the exact row the write side just inserted.
        let read_handle = orchestrator.view_store();
        assert_eq!(
            read_handle.get(&view_id),
            Some(view),
            "get_view's view_store() call must observe the SAME store a tool's ViewSink wrote \
             to, not an independent instance"
        );

        shutdown.cancel();
        tasks.close();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), tasks.wait()).await;
    }
}

#[cfg(test)]
mod view_store_quarantine_tests {
    //! View Engine Phase A (phase 3), SEC F2: `ViewStore::get` — and the `view_store()` handle
    //! that exposes it — is GUI-consumption-only. `Orchestrator`'s `view_store` field is
    //! private, but private in Rust means "visible in the defining module and every descendant
    //! module" — since `Orchestrator` is defined at the crate root, every module in this crate
    //! is technically able to reach it. Only a convention (never call `view_store()` from
    //! agent/tool code — read a `DataView` back into a prompt without the renderer's escaping
    //! contract) stands between a future call site and a silent violation, so this test turns
    //! that convention into a regression guard: no `haily-core` or `haily-tools` production
    //! file may call `.view_store(`. `haily-tools` cannot even name `ViewStore` (it has no
    //! dependency on `haily-core`, which is what avoids the layering cycle in the first place),
    //! so the `haily-tools` half of this scan is defense-in-depth against a future dependency
    //! change rather than a live risk today.
    use std::path::Path;

    /// `exclude_names` skips whole files by base name (e.g. `lib.rs`, where the handle is
    /// DEFINED and where `view_store_sharing_tests` legitimately calls it to prove the
    /// getter's own sharing contract — a meta-test of the accessor, not a consumption site).
    /// Comment lines (trimmed prefix `//`, covering `//`/`///`/`//!`) are skipped so prose
    /// explaining the getter's contract (as this very module does) cannot self-trip the scan
    /// — only an actual call in code counts as an offense.
    fn scan_dir_for(dir: &Path, needle: &str, exclude_names: &[&str], offenders: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                scan_dir_for(&path, needle, exclude_names, offenders);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let is_excluded = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| exclude_names.contains(&n));
                if is_excluded {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let has_call = text
                        .lines()
                        .any(|line| !line.trim_start().starts_with("//") && line.contains(needle));
                    if has_call {
                        offenders.push(path.display().to_string());
                    }
                }
            }
        }
    }

    #[test]
    fn no_core_or_tools_caller_of_view_store_handle() {
        let core_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let tools_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../haily-tools/src");
        let mut offenders = Vec::new();
        // ".view_store(" (leading dot) matches only a METHOD CALL on the handle, not this
        // module's own `fn view_store(&self)` definition line.
        scan_dir_for(&core_src, ".view_store(", &["lib.rs"], &mut offenders);
        scan_dir_for(&tools_src, ".view_store(", &[], &mut offenders);
        assert!(
            offenders.is_empty(),
            "ViewStore::get is GUI-consumption-only (SEC F2) — found a core/tools caller of \
             the view_store() handle in: {offenders:?}"
        );
    }
}
