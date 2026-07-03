pub mod connector;
pub mod journal_undo;
pub mod security;
pub mod v1;

use anyhow::Result;
use async_trait::async_trait;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_types::ApprovalGate;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct ToolContext {
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    pub session_id: Uuid,
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
    /// Local write with no external side effect and no undo path yet (no v1 journal —
    /// the journal wraps connectors only). Executes without an approval prompt.
    ReversibleWrite,
    /// Requires human approval before executing: external egress, or a local
    /// operation gated for safety even though it may be physically reversible
    /// (soft-deletes are `IrreversibleWrite` for GATING, not reversibility — never
    /// re-tier them to `ReversibleWrite` until a journal/undo path covers them).
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
            Arc::new(WorktreeApplyTool) as Arc<dyn Tool>,
        ] {
            reg.register(tool);
        }
        // Undo tool for the action journal (Safe Operator Harness phase 3). Registered
        // with a fail-closed placeholder executor; phase 4 re-registers it with the real
        // HTTP executor. `IrreversibleWrite` + kill-switch-EXEMPT (see `journal_undo`).
        reg.register(Arc::new(journal_undo::JournalUndoTool {
            executor: Arc::new(connector::UnconfiguredExecutor),
        }));
        reg
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
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
}
