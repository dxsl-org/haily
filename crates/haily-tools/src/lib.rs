pub mod security;
pub mod v1;

use anyhow::Result;
use async_trait::async_trait;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use std::sync::Arc;
use uuid::Uuid;

pub struct ToolContext {
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    pub session_id: Uuid,
    /// Agent nesting depth: 0 = L0 orchestrator, 1 = L1 domain agent, 2 = L2 specialist.
    /// Delegate tools check this to enforce max depth and prevent infinite recursion.
    pub depth: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolClass {
    AutoApprove,
    RequireApproval,
    Blocked,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    fn approval_class(&self) -> ToolClass;
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String>;
}

pub struct ToolRegistry {
    tools: std::collections::HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: std::collections::HashMap::new() }
    }

    /// Register all V1 tools.
    pub fn build_v1() -> Self {
        let mut reg = Self::new();
        use v1::{
            calendar::*, memory::*, notes::*, reminders::*, tasks::*, web::*,
            work_items::*, worktree_tool::*,
        };
        for tool in [
            Arc::new(WebSearchTool)   as Arc<dyn Tool>,
            Arc::new(UrlFetchTool)    as Arc<dyn Tool>,
            Arc::new(HttpRequestTool) as Arc<dyn Tool>,
            Arc::new(CalendarListTool)   as Arc<dyn Tool>,
            Arc::new(CalendarAddTool)    as Arc<dyn Tool>,
            Arc::new(CalendarDeleteTool) as Arc<dyn Tool>,
            Arc::new(NoteSaveTool)    as Arc<dyn Tool>,
            Arc::new(NoteSearchTool)  as Arc<dyn Tool>,
            Arc::new(NoteUpdateTool)  as Arc<dyn Tool>,
            Arc::new(NoteDeleteTool)  as Arc<dyn Tool>,
            Arc::new(ReminderAddTool)    as Arc<dyn Tool>,
            Arc::new(ReminderListTool)   as Arc<dyn Tool>,
            Arc::new(ReminderDeleteTool) as Arc<dyn Tool>,
            Arc::new(TaskCreateTool)   as Arc<dyn Tool>,
            Arc::new(TaskListTool)     as Arc<dyn Tool>,
            Arc::new(TaskCompleteTool) as Arc<dyn Tool>,
            Arc::new(TaskDeleteTool)   as Arc<dyn Tool>,
            Arc::new(MemoryRememberTool) as Arc<dyn Tool>,
            Arc::new(MemorySearchTool)   as Arc<dyn Tool>,
            Arc::new(MemoryListTool)     as Arc<dyn Tool>,
            Arc::new(MemoryForgetTool)   as Arc<dyn Tool>,
            Arc::new(FeedbackReactTool)  as Arc<dyn Tool>,
            Arc::new(WorkItemListTool)   as Arc<dyn Tool>,
            Arc::new(WorkItemResumeTool) as Arc<dyn Tool>,
            Arc::new(WorktreeApplyTool)  as Arc<dyn Tool>,
        ] {
            reg.register(tool);
        }
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
        fn name(&self) -> &str { self.0 }
        fn description(&self) -> &str { "mock" }
        fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({}) }
        fn approval_class(&self) -> ToolClass { ToolClass::AutoApprove }
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
            "web_search", "memory_search", "memory_remember",
            "reminder_add", "calendar_list", "note_save",
            "work_item_list", "work_item_resume", "feedback_react",
        ] {
            assert!(base.get(name).is_some(), "missing quick tool: {name}");
        }
    }
}
