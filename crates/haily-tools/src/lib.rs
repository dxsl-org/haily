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
