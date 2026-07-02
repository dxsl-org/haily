//! Shared message/DTO types passed across the `haily-core` ↔ `haily-io` channel boundary.
//!
//! This crate is intentionally a leaf: types + derives only, no logic. It exists so that
//! `haily-core` (agent/orchestrator logic) and `haily-io` (adapters) can both depend on the
//! same message shapes without `haily-core` importing the adapter layer — see CLAUDE.md's
//! "haily-core must never import from haily-io" invariant.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub session_id: Uuid,
    pub adapter_id: String,
    pub message: String,
    pub user_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ResponseChunk {
    Text(String),
    ToolApprovalRequest {
        tool: String,
        args: String,
        approval_id: Uuid,
    },
    ToolResult {
        name: String,
        ok: bool,
    },
    /// Turn-ending failure (LLM error, cancelled mid-stream, etc.), distinct from
    /// `Text` specifically so adapters that BUFFER `Text` chunks until `Complete`
    /// (`haily-io::telegram`) can tell "discard the partial buffer, show only this
    /// error" apart from "append this to the buffer" — conflating the two produced a
    /// real bug (phase-06 red team): a partial answer plus an error both arriving as
    /// `Text` fused into one "partial-answer⚠️error" message on Complete. Adapters
    /// that don't buffer (CLI, GUI) may treat this the same as `Text` for rendering.
    Error(String),
    Complete,
}

/// Snapshot of a single active work item for display in adapters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItemStatus {
    pub title: String,
    pub status: String,
    pub progress: u8,
    pub phase: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Notification {
    MorningBrief(String),
    Alert {
        title: String,
        body: String,
        urgent: bool,
    },
    ReminderFired {
        reminder_id: Uuid,
        title: String,
    },
    /// Broadcast when the set of active work items changes (added, progressed, or removed).
    WorkItemsChanged(Vec<WorkItemStatus>),
}

pub type RequestSender = tokio::sync::mpsc::Sender<Request>;
pub type RequestReceiver = tokio::sync::mpsc::Receiver<Request>;

/// Adapter-facing half of the tool-approval flow. Lives here (not `haily-core`) so
/// `haily-io` adapters can resolve a pending approval without depending on
/// `haily-core` — see CLAUDE.md's layering invariant. `haily-core::ApprovalBroker`
/// is the sole implementer; adapters hold it as `Arc<dyn ApprovalResolver>`.
///
/// `approval_id` is shown to the user (not a secret) — `session_id` is the actual
/// auth boundary, so implementations MUST verify the pending approval was registered
/// under this exact `session_id` before honoring `approved`.
pub trait ApprovalResolver: Send + Sync {
    /// Resolve a pending approval. Returns `true` if a matching pending approval was
    /// found for `session_id` and resolved by this call, `false` if `approval_id` is
    /// unknown, already resolved, or bound to a different session (forged/foreign-chat
    /// attempt — callers should log and otherwise ignore a `false` result).
    fn resolve(&self, approval_id: Uuid, session_id: Uuid, approved: bool) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_chunk_serde_roundtrip_preserves_tag_and_content() {
        let chunk = ResponseChunk::ToolApprovalRequest {
            tool: "exec".to_string(),
            args: "{}".to_string(),
            approval_id: Uuid::nil(),
        };
        let json = serde_json::to_string(&chunk).expect("serialize");
        // Frontend (src/lib/tauri.ts) depends on this exact envelope shape.
        assert!(json.contains("\"type\":\"ToolApprovalRequest\""));
        assert!(json.contains("\"data\":"));

        let round_tripped: ResponseChunk = serde_json::from_str(&json).expect("deserialize");
        match round_tripped {
            ResponseChunk::ToolApprovalRequest {
                tool,
                args,
                approval_id,
            } => {
                assert_eq!(tool, "exec");
                assert_eq!(args, "{}");
                assert_eq!(approval_id, Uuid::nil());
            }
            other => panic!("unexpected variant after roundtrip: {other:?}"),
        }
    }

    #[test]
    fn response_chunk_error_variant_roundtrips_and_is_distinct_from_text() {
        // The frontend's discriminated union (`src/lib/tauri.ts`) and
        // `haily-io::telegram`'s buffer-discard logic both depend on `Error` never
        // collapsing into the same wire tag as `Text`.
        let chunk = ResponseChunk::Error("boom".to_string());
        let json = serde_json::to_string(&chunk).expect("serialize");
        assert!(json.contains("\"type\":\"Error\""));
        assert!(!json.contains("\"type\":\"Text\""));

        let round_tripped: ResponseChunk = serde_json::from_str(&json).expect("deserialize");
        match round_tripped {
            ResponseChunk::Error(msg) => assert_eq!(msg, "boom"),
            other => panic!("unexpected variant after roundtrip: {other:?}"),
        }
    }

    #[test]
    fn work_items_changed_notification_roundtrip() {
        let notif = Notification::WorkItemsChanged(vec![WorkItemStatus {
            title: "test".to_string(),
            status: "running".to_string(),
            progress: 50,
            phase: Some("build".to_string()),
        }]);
        let json = serde_json::to_string(&notif).expect("serialize");
        let round_tripped: Notification = serde_json::from_str(&json).expect("deserialize");
        match round_tripped {
            Notification::WorkItemsChanged(items) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].progress, 50);
            }
            other => panic!("unexpected variant after roundtrip: {other:?}"),
        }
    }
}
