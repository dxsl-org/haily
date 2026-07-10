pub mod coding;
pub mod connector;
pub mod exec;
pub mod journal_undo;
pub mod schedule;
pub mod security;
pub mod skill_fetch;
pub mod v1;

use anyhow::Result;
use async_trait::async_trait;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_types::ApprovalGate;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Days a LOCAL tool's (tasks/notes/reminders) journal row is retained before purge — mirrors
/// the connector path's `CONNECTOR_RETENTION_DAYS` (USER-VALIDATED parity; this phase does
/// NOT change retention policy or re-tier any local tool, it only journals them).
pub const LOCAL_RETENTION_DAYS: i64 = 30;

/// Per-turn ceiling on auto-run (no-approval-prompt) deletes of a re-tiered `ReversibleWrite`
/// soft-delete tool — USER-VALIDATED (2026-07-03 interview), declared as a const rather than
/// hard-coded at the dispatch call site. Beyond this count, `haily-core::tool_call::dispatch`
/// escalates the NEXT such delete to the approval gate for that one call, as a DISPATCH-layer
/// policy — the tool's own `risk_tier()` return value is unchanged (see the `RiskTier` doc
/// below on why this must never become an args-dependent tier).
pub const MAX_AUTO_DELETES_PER_TURN: usize = 5;

pub struct ToolContext {
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    pub session_id: Uuid,
    /// Server-derived correlation id shared by every tool call within ONE agent turn —
    /// minted exactly once per turn in `agent::run_turn`/`run_sub_turn` (NEVER from LLM
    /// output or tool args, so a compromised sub-agent cannot forge another turn's
    /// group). A delegated sub-turn reuses its PARENT's `turn_id` rather than minting a
    /// fresh one (see `delegate.rs`) — a delegation is part of the turn that requested
    /// it, not a new logical unit of work, so its writes must undo together with the
    /// parent's under one `undo_turn` call. Stamped onto every journal row (local and
    /// connector) so `journal::list_by_turn` can collect the group.
    pub turn_id: Uuid,
    /// Agent nesting depth: 0 = L0 orchestrator, 1 = L1 domain agent, 2 = L2 specialist.
    /// Delegate tools check this to enforce max depth and prevent infinite recursion.
    pub depth: u8,
    /// Static domain label of the (sub-)agent this context runs in, e.g.
    /// `Some("developer")` for an L1 developer sub-turn. `None` at L0 (the root
    /// orchestrator has no single domain). Used SERVER-SIDE only to build the
    /// display-only `origin` on an approval request (`L{depth}:{domain}`) — never an
    /// auth input, and never sourced from LLM/task text.
    pub domain: Option<&'static str>,
    /// Seam handle for raising a tool approval from wherever this `ToolContext` is
    /// used (L0 or a sub-turn) without `haily-tools` depending on `haily-core` — the
    /// trait lives in the leaf `haily-types` crate. At L0 this is the real
    /// `ApprovalBroker`; at a sub-turn it is the SAME broker threaded down, so an
    /// approval reaches the one user via the one session broker at any depth.
    pub approval_gate: Arc<dyn ApprovalGate>,
    /// Channel `dispatch` sends `ResponseChunk::ToolApprovalRequest`/`ToolResult` up.
    /// At L0 this is the turn's real response stream; at a sub-turn it is a local
    /// channel whose receiver a forwarder drains, relaying ONLY approval requests to
    /// the parent (sub-agent narration stays discarded).
    pub approval_tx: tokio::sync::mpsc::Sender<haily_types::ResponseChunk>,
    /// This (sub-)turn's cancellation token — fired on shutdown so a pending approval
    /// raised through the seam never blocks the drain. At a sub-turn this is a
    /// `child_token()` of the parent's, so a sub-turn timeout cancels only itself.
    pub cancel: CancellationToken,
    /// Count of successful re-tiered-delete executions so far THIS TURN (M2) — shared
    /// with every sub-turn `delegate.rs` spawns (same `turn_id` group), so a delegated
    /// sub-agent's deletes count toward the SAME cap as the parent's, not a fresh one.
    /// Incremented ONLY by `haily-core::tool_call::dispatch` after a re-tiered delete
    /// actually executes — never by a tool itself, and never resettable from LLM/task
    /// text, so a compromised sub-agent cannot reset or bypass the cap.
    pub turn_deletes: Arc<std::sync::atomic::AtomicUsize>,
    /// M4 out-param side-channel (Harness Completion phase 3, R4 framing): threads a
    /// journal row id out of a local tool's `execute()` without widening the
    /// `Tool::execute` trait's `Result<String>` return across ~20 implementations.
    /// Set by `local_journaled_write`'s local-tool callers (`v1::{tasks,notes,
    /// reminders}`) AFTER `set_post_state_version` has landed inside the SAME
    /// transaction `local_journaled_write` already commits — so a `Some` value here
    /// always implies the C10 undo-guard's baseline version is recorded (see that
    /// function's doc comment). `dispatch` resets this to `None` at the TOP of every
    /// call (never carried over from a prior tool) and reads it AFTER `execute()`
    /// returns, populating `ResponseChunk::ToolResult{reversible, journal_id}`. This
    /// is PER-DISPATCH-CALL state, not a process-global: dispatch is sequential
    /// within one turn, so reset-then-read around a single `execute()` call can never
    /// observe another call's value (see `tool_call.rs`'s no-cross-tool-bleed test).
    pub last_journal_id: Arc<std::sync::Mutex<Option<String>>>,
}

/// Blast-radius classification for a tool call, evaluated per-call against `args` so
/// a single tool CAN return different tiers for different arguments (e.g. a future
/// "delete draft" vs "delete sent" distinction) — v1 tools are constant-tier (YAGNI:
/// no arg-branching added yet), which is what makes the `auto_approve` empty-probe
/// validation sound (see `no_v1_tool_tier_varies_by_args`).
///
/// Fail-closed contract: a tool that cannot parse `args` well enough to determine its
/// true tier MUST return `IrreversibleWrite`, never a cheaper tier — an unparseable
/// call is exactly the case where blast radius is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskTier {
    /// Pure read, no side effect.
    Read,
    /// A write that executes WITHOUT an approval prompt because it is journaled and
    /// undoable. Covers every plain local create/update (calendar_add, note_save,
    /// note_update, memory_remember, reminder_add, task_create, task_complete,
    /// work_item_resume) AND, as of the Harness Completion phase (re-tier +
    /// turn_id group undo), Phase 12 (KmsHandle-aware compensator), Phase 11
    /// assistant-depth (work_items closes its harness gap), and Phase 13b
    /// assistant-depth (calendar occurrence-vs-series undo + exceptions), the SIX
    /// local soft-delete tools whose journal/undo coverage now matches: `task_delete`,
    /// `note_delete`, `reminder_delete`, `memory_forget`, `work_item_delete`,
    /// `calendar_delete`. `memory_forget`'s undo is a DISTINCT KMS-aware compensator
    /// (`KmsHandle::restore_fact`), not the generic `restore_row`, because it must
    /// ALSO re-insert/un-tombstone the fact's vector in the live HNSW index — see
    /// `journal_undo::local_compensator`'s `LocalTable::KmsFacts` branch.
    /// `calendar_delete` is ALSO not purely generic: its `scope='occurrence'` path
    /// undoes via a THIRD distinct compensator arm (`LocalOpKind::DeleteOccurrence` —
    /// removes an exception row from `calendar_exceptions`, a table separate from the
    /// event row itself), while its `scope='series'` path undoes via the fully
    /// generic snapshot compensator, same as `task_delete`. The remaining three
    /// (`task_delete`/`note_delete`/`work_item_delete`) undo via that same fully
    /// generic snapshot compensator. Their safety net is no longer the approval
    /// prompt but: (1) the journal + undo path, (2) a per-turn destructive-op cap
    /// enforced in DISPATCH, not here (`MAX_AUTO_DELETES_PER_TURN` — see its doc;
    /// `haily-core::tool_call::RETIERED_DELETE_TOOLS` MUST list every tool in this
    /// covered set — C1), and (3) the kill switch (C8), which still blocks every
    /// `ReversibleWrite` exactly as it blocks `IrreversibleWrite`. A tool must NEVER
    /// vary this return by args (see the fail-closed contract above) — the cap's
    /// escalation happens in `haily-core::tool_call::dispatch`, which treats an
    /// over-cap call as `IrreversibleWrite` FOR THAT CALL ONLY, without this method's
    /// return value ever changing.
    ReversibleWrite,
    /// Requires human approval before executing: external egress, or a local
    /// operation gated for safety even though it may be physically reversible.
    IrreversibleWrite,
    /// Never executes.
    Blocked,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    /// Classify this call's blast radius. See `RiskTier`'s fail-closed contract for
    /// the malformed-args case.
    fn risk_tier(&self, args: &serde_json::Value) -> RiskTier;
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String>;
}

pub struct ToolRegistry {
    tools: std::collections::HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: std::collections::HashMap::new(),
        }
    }

    /// Register all V1 tools.
    pub fn build_v1() -> Self {
        let mut reg = Self::new();
        use coding::*;
        use v1::{
            calendar::*, memory::*, notes::*, reminders::*, tasks::*, web::*, work_items::*,
            worktree_tool::*,
        };
        for tool in [
            Arc::new(WebSearchTool) as Arc<dyn Tool>,
            Arc::new(UrlFetchTool) as Arc<dyn Tool>,
            Arc::new(HttpRequestTool) as Arc<dyn Tool>,
            Arc::new(CalendarListTool) as Arc<dyn Tool>,
            Arc::new(CalendarAddTool) as Arc<dyn Tool>,
            Arc::new(CalendarDeleteTool) as Arc<dyn Tool>,
            Arc::new(NoteSaveTool) as Arc<dyn Tool>,
            Arc::new(NoteSearchTool) as Arc<dyn Tool>,
            Arc::new(NoteUpdateTool) as Arc<dyn Tool>,
            Arc::new(NoteDeleteTool) as Arc<dyn Tool>,
            Arc::new(ReminderAddTool) as Arc<dyn Tool>,
            Arc::new(ReminderListTool) as Arc<dyn Tool>,
            Arc::new(ReminderDeleteTool) as Arc<dyn Tool>,
            Arc::new(TaskCreateTool) as Arc<dyn Tool>,
            Arc::new(TaskListTool) as Arc<dyn Tool>,
            Arc::new(TaskCompleteTool) as Arc<dyn Tool>,
            Arc::new(TaskDeleteTool) as Arc<dyn Tool>,
            Arc::new(MemoryRememberTool) as Arc<dyn Tool>,
            Arc::new(MemorySearchTool) as Arc<dyn Tool>,
            Arc::new(MemoryListTool) as Arc<dyn Tool>,
            Arc::new(MemoryForgetTool) as Arc<dyn Tool>,
            Arc::new(FeedbackReactTool) as Arc<dyn Tool>,
            Arc::new(WorkItemListTool) as Arc<dyn Tool>,
            Arc::new(WorkItemResumeTool) as Arc<dyn Tool>,
            Arc::new(WorkItemDeleteTool) as Arc<dyn Tool>,
            Arc::new(WorktreeApplyTool) as Arc<dyn Tool>,
            // Coding tool surface (Sub-Agent + Skill Architecture phase 1) — registered here,
            // whitelisted only for the developer domain + coding specialists via sub_registry.
            Arc::new(FsReadTool) as Arc<dyn Tool>,
            Arc::new(FsListTool) as Arc<dyn Tool>,
            Arc::new(FsGrepTool) as Arc<dyn Tool>,
            Arc::new(FsWriteTool) as Arc<dyn Tool>,
            Arc::new(FsEditTool) as Arc<dyn Tool>,
            Arc::new(FsMoveTool) as Arc<dyn Tool>,
            Arc::new(FsDeleteTool) as Arc<dyn Tool>,
            Arc::new(ShellExecTool) as Arc<dyn Tool>,
            Arc::new(GitStatusTool) as Arc<dyn Tool>,
            Arc::new(GitDiffTool) as Arc<dyn Tool>,
            Arc::new(GitCommitTool) as Arc<dyn Tool>,
            Arc::new(crate::exec::code_exec::CodeExecTool) as Arc<dyn Tool>,
            // Authored-skill discovery + lazy-load (Sub-Agent + Skill Architecture phase 2)
            // — universal Read-tier tools, whitelisted for L0 + every domain + specialist.
            Arc::new(skill_fetch::SkillSearchTool) as Arc<dyn Tool>,
            Arc::new(skill_fetch::SkillListSectionsTool) as Arc<dyn Tool>,
            Arc::new(skill_fetch::SkillFetchTool) as Arc<dyn Tool>,
        ] {
            reg.register(tool);
        }
        // Undo tool for the action journal (Safe Operator Harness phase 3). Registered
        // with an EMPTY routing table (every connector op resolves to "no executor" —
        // fail-closed, mirrors the old `UnconfiguredExecutor` placeholder's intent);
        // `register_connectors` (M5c, Activate-and-Measure phase 4b) re-registers it with
        // the real per-op routing built from whatever manifests are actually approved.
        // `IrreversibleWrite` + kill-switch-EXEMPT (see `journal_undo`).
        reg.register(Arc::new(journal_undo::JournalUndoTool {
            resolver: journal_undo::ConnectorResolver::new(),
        }));
        reg
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Register one `HttpConnectorTool` per op of each parsed, human-approved manifest
    /// (Safe Operator Harness phase 4, R3). MUST be called on the `base_v1` registry
    /// BEFORE any `sub_registry()` snapshot / `Arc`-wrap (C2): `register` needs unique
    /// ownership (`&mut self`), and once `base_v1` is snapshotted into per-domain
    /// sub-registries and frozen behind an `Arc`, no connector tool could be added and the
    /// domain whitelists would resolve their connector op-names to `None`.
    ///
    /// `kill` is the SAME `Arc<AtomicBool>` the orchestrator flips live — cloned into each
    /// tool AND its `HttpExecutor` so the M5 re-check observes a mid-write kill at any
    /// depth. `cred_ref_for` yields the credential preference-key name per connector so the
    /// journal records WHICH credential a write used (the secret itself is redacted, C4).
    /// `timeout` bounds every external connector call. `credential_getter` (phase 2) is
    /// injected into every `HttpExecutor` so it can apply a manifest's declared `auth`
    /// section; `None` preserves pre-phase-2 behavior (a manifest with no `auth` section is
    /// unaffected either way — the getter is only ever consulted when `auth` is present).
    ///
    /// `manifests` pairs each parsed manifest with its `ConnectorManifestRow::content_hash`
    /// (M2, Activate-and-Measure phase 4b) — pinned into every op's `HttpConnectorTool` so
    /// outbox rows record which exact manifest version they wrote against.
    ///
    /// M5c: builds the op→executor routing table this method RETURNS (a
    /// `journal_undo::ConnectorResolver`, wrapping the concrete map — no "inject a resolver"
    /// abstraction), then re-registers `JournalUndoTool` with it — `register` performs a
    /// `HashMap` insert, so this OVERWRITES the fail-closed empty-routing placeholder
    /// `build_v1` sealed (or, if called more than once, a prior call's routing — harmless;
    /// production calls this exactly once at startup). The caller (`Orchestrator::init`)
    /// also hands the returned table to the startup reconcile sweep.
    pub fn register_connectors(
        &mut self,
        manifests: Vec<(connector::Manifest, String)>,
        kill: Arc<std::sync::atomic::AtomicBool>,
        timeout: std::time::Duration,
        cred_ref_for: impl Fn(&str) -> String,
        credential_getter: Option<Arc<dyn connector::CredentialGetter>>,
    ) -> journal_undo::ConnectorResolver {
        let mut routing = journal_undo::ConnectorResolver::new();
        for (manifest, content_hash) in manifests {
            let manifest = Arc::new(manifest);
            let cred_ref = cred_ref_for(&manifest.connector_name);
            // One executor shared across all ops of a manifest (same base_url + allowance).
            let shared_executor: Arc<dyn crate::connector::ConnectorExecutor> =
                Arc::new(connector::HttpExecutor::new(
                    connector::HttpExecutorConfig::production(
                        Arc::clone(&manifest),
                        Arc::clone(&kill),
                        timeout,
                    )
                    .with_credential_getter(credential_getter.clone()),
                ));
            for op in &manifest.ops {
                self.register(Arc::new(connector::HttpConnectorTool {
                    manifest: Arc::clone(&manifest),
                    op: Arc::new(op.clone()),
                    executor: Arc::clone(&shared_executor),
                    kill: Arc::clone(&kill),
                    cred_ref: cred_ref.clone(),
                    manifest_hash: content_hash.clone(),
                }));
            }
            routing.merge(journal_undo::ConnectorResolver::for_manifest(
                &manifest,
                shared_executor,
                content_hash,
            ));
        }
        self.register(Arc::new(journal_undo::JournalUndoTool {
            resolver: routing.clone(),
        }));
        routing
    }

    /// Build a sub-registry containing only the named tools.
    /// Used by delegate tools to enforce per-domain tool whitelists.
    /// Unknown names are silently skipped.
    pub fn sub_registry(&self, allowed: &[&str]) -> Self {
        let mut reg = Self::new();
        for name in allowed {
            if let Some(tool) = self.tools.get(*name) {
                reg.tools.insert((*name).to_string(), Arc::clone(tool));
            }
        }
        reg
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn list(&self) -> Vec<&Arc<dyn Tool>> {
        self.tools.values().collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::build_v1()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockTool(&'static str);

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "mock"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn risk_tier(&self, _args: &serde_json::Value) -> RiskTier {
            RiskTier::Read
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<String> {
            Ok("ok".into())
        }
    }

    fn registry_with(names: &[&'static str]) -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        for n in names {
            reg.register(Arc::new(MockTool(n)));
        }
        reg
    }

    #[test]
    fn sub_registry_keeps_only_whitelisted() {
        let base = registry_with(&["a", "b", "c", "d"]);
        let sub = base.sub_registry(&["a", "c"]);
        assert_eq!(sub.len(), 2);
        assert!(sub.get("a").is_some());
        assert!(sub.get("c").is_some());
        assert!(sub.get("b").is_none());
    }

    #[test]
    fn sub_registry_silently_skips_unknown_names() {
        let base = registry_with(&["a", "b"]);
        let sub = base.sub_registry(&["a", "does_not_exist"]);
        assert_eq!(sub.len(), 1);
        assert!(sub.get("a").is_some());
        assert!(sub.get("does_not_exist").is_none());
    }

    #[test]
    fn sub_registry_empty_whitelist_yields_empty() {
        let base = registry_with(&["a", "b"]);
        let sub = base.sub_registry(&[]);
        assert!(sub.is_empty());
    }

    #[test]
    fn build_v1_registers_all_quick_tools() {
        // Guards against silent whitelist drift: the L0 quick-tool names the
        // orchestrator relies on must all exist in the base V1 registry.
        let base = ToolRegistry::build_v1();
        for name in [
            "web_search",
            "memory_search",
            "memory_remember",
            "reminder_add",
            "calendar_list",
            "note_save",
            "work_item_list",
            "work_item_resume",
            "feedback_react",
            // Phase 2 skill lazy-load tools — also L0 quick tools.
            "skill_search",
            "skill_list_sections",
            "skill_fetch",
        ] {
            assert!(base.get(name).is_some(), "missing quick tool: {name}");
        }
    }

    /// A tool that parses `args["limit"]` and demonstrates the `RiskTier` fail-closed
    /// contract: a well-formed numeric limit is a plain read, but an unparseable
    /// value means the tool cannot determine its true blast radius, so it MUST
    /// report `IrreversibleWrite` rather than silently falling back to `Read`.
    struct LimitParsingTool;

    #[async_trait]
    impl Tool for LimitParsingTool {
        fn name(&self) -> &str {
            "limit_parsing_test_tool"
        }
        fn description(&self) -> &str {
            "test tool for the fail-closed contract"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn risk_tier(&self, args: &serde_json::Value) -> RiskTier {
            match args.get("limit") {
                None => RiskTier::Read, // absent is a valid default, not malformed
                Some(v) if v.is_u64() => RiskTier::Read,
                // Present but not a valid unsigned integer: blast radius unknown.
                Some(_) => RiskTier::IrreversibleWrite,
            }
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<String> {
            Ok("ok".into())
        }
    }

    #[test]
    fn risk_tier_fail_closed_on_malformed_args() {
        let tool = LimitParsingTool;
        assert_eq!(
            tool.risk_tier(&serde_json::json!({"limit": "not-a-number"})),
            RiskTier::IrreversibleWrite
        );
        assert_eq!(
            tool.risk_tier(&serde_json::json!({"limit": 10})),
            RiskTier::Read
        );
        assert_eq!(tool.risk_tier(&serde_json::json!({})), RiskTier::Read);
    }

    #[test]
    fn register_connectors_before_sub_registry_makes_ops_resolvable() {
        // C2 ordering at the tools layer: registering connector ops into base_v1 BEFORE
        // taking a sub_registry snapshot lets the snapshot resolve the op-names. This is
        // the same ordering the orchestrator's `connector_op_visible_to_delegable_subagent`
        // test asserts end-to-end.
        let manifest = connector::manifest::parse(
            r#"{"connector_name":"odoo","version":"1","base_url":"https://erp.example.com",
                "allowed_ip_cidrs":[],
                "ops":[{"name":"odoo_contact_create","risk_tier":"IrreversibleWrite",
                        "compensability":"compensatable","compensation":{"op":"unlink"}}]}"#,
        )
        .unwrap();
        let mut base = ToolRegistry::build_v1();
        base.register_connectors(
            vec![(manifest, "test-hash".to_string())],
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::time::Duration::from_secs(30),
            |c| format!("{c}.api_key"),
            None,
        );
        // Now visible in base AND in a whitelist-scoped sub_registry that names the op.
        assert!(base.get("odoo_contact_create").is_some());
        let sub = base.sub_registry(&["odoo_contact_create", "web_search"]);
        assert!(
            sub.get("odoo_contact_create").is_some(),
            "connector op must resolve in a sub_registry snapshot taken AFTER registration"
        );
    }
}
