//! Shared message/DTO types passed across the `haily-core` ↔ `haily-io` channel boundary.
//!
//! This crate is intentionally a leaf: types + derives only, no logic. It exists so that
//! `haily-core` (agent/orchestrator logic) and `haily-io` (adapters) can both depend on the
//! same message shapes without `haily-core` importing the adapter layer — see CLAUDE.md's
//! "haily-core must never import from haily-io" invariant.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
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
        /// Server-derived "who is asking" label (e.g. `"L0"`, `"L1:developer"`),
        /// display-only. NEVER an auth input — `session_id` is the sole approval
        /// boundary; `origin` is derived from `ctx.depth` + a static domain name in
        /// `tool_call::dispatch`, never from LLM/task text. `#[serde(default)]` keeps
        /// this ADDITIVE: a pre-`origin` payload (no field) still deserializes to
        /// `None`, so the wire envelope is not a breaking change (M8).
        #[serde(default)]
        origin: Option<String>,
        /// True when the underlying tool is normally `ReversibleWrite` and this
        /// specific approval prompt exists ONLY because the per-turn destructive-op
        /// cap escalated it for this call (Harness Completion phase 2's M2 policy) —
        /// i.e. the action IS journaled/undoable, this is not a genuinely final write.
        /// `false` for a tool that is actually `IrreversibleWrite`/`Blocked` on its own
        /// merits (e.g. `memory_forget`, `calendar_delete`). Display-only — lets a UI
        /// distinguish "can't be undone" from "cap reached, please confirm" without
        /// re-deriving tier logic client-side. `#[serde(default)]` keeps this ADDITIVE.
        #[serde(default)]
        reversible: bool,
    },
    ToolResult {
        name: String,
        ok: bool,
        /// Whether this call was a journaled, undoable `ReversibleWrite` local write
        /// (Harness Completion phase 3, R4 framing) — `false` for `Read`/
        /// `IrreversibleWrite` tools and for any `ReversibleWrite` that did not go
        /// through the local journal out-param (`ToolContext::last_journal_id`).
        /// `#[serde(default)]` keeps this ADDITIVE: a pre-this-change payload with no
        /// `reversible` key still deserializes, defaulting to `false` (M8).
        #[serde(default)]
        reversible: bool,
        /// The action-journal row id to pass to `journal_undo` for this call, set
        /// ONLY once `dispatch` observed `ToolContext::last_journal_id` populated
        /// AFTER `execute()` returned — which in turn only happens once
        /// `local_journaled_write` has committed its transaction with
        /// `post_state_version` recorded (see that function's doc comment). A
        /// `journal_id` therefore always implies the C10 undo-guard's baseline
        /// version has landed. `#[serde(default)]` keeps this ADDITIVE (M8): an old
        /// payload with no `journal_id` key deserializes to `None`.
        #[serde(default)]
        journal_id: Option<String>,
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

/// Request-side half of the tool-approval flow, mirroring the `ApprovalResolver` /
/// `haily-core::ApprovalBroker` split: this trait lives in the leaf crate so
/// `haily-tools` (and any sub-agent code built on it) can raise an approval without
/// depending on `haily-core` — see CLAUDE.md's layering invariant.
/// `haily-core::ApprovalBroker` is the sole implementer.
///
/// `approval_id` is shown to the user (not a secret); `session_id` is the sole auth
/// boundary — implementations MUST verify the pending approval was registered under
/// this exact `session_id` before honoring a decision (mirrors `ApprovalResolver`).
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    /// Register a pending approval and wait for a decision. Returns `true` only if
    /// approved before `cancel` fires or the implementation's own timeout elapses —
    /// callers must treat cancellation and timeout identically to an explicit deny.
    async fn request(&self, approval_id: Uuid, session_id: Uuid, cancel: &CancellationToken) -> bool;

    /// Whether `tool_name` is on the pre-validated auto-approve allowlist and may skip
    /// the interactive prompt. Exposed on the gate (not just the concrete broker) so
    /// `dispatch` can consult it through the SAME `Arc<dyn ApprovalGate>` seam handle
    /// it uses for `request` — at any depth. Every bypass is logged at warn by the
    /// caller; the allowlist can never contain a destructive/exfil tool (validated at
    /// bootstrap). Default `false`: a gate with no allowlist auto-approves nothing.
    fn is_auto_approved(&self, _tool_name: &str) -> bool {
        false
    }
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
            origin: None,
            reversible: false,
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
                origin,
                reversible,
            } => {
                assert_eq!(tool, "exec");
                assert_eq!(args, "{}");
                assert_eq!(approval_id, Uuid::nil());
                assert_eq!(origin, None);
                assert!(!reversible);
            }
            other => panic!("unexpected variant after roundtrip: {other:?}"),
        }
    }

    /// M8: `origin` is `Option<String>` + `#[serde(default)]`, so it is ADDITIVE —
    /// a payload minted before the field existed (no `origin` key at all) must still
    /// deserialize, defaulting to `None`. This is the guarantee that a persisted or
    /// in-flight old chunk does not break after upgrade.
    #[test]
    fn origin_absent_payload_deserializes() {
        // Exactly the pre-`origin` wire shape — note NO `origin` key in `data`.
        let legacy = r#"{"type":"ToolApprovalRequest","data":{"tool":"exec","args":"{}","approval_id":"00000000-0000-0000-0000-000000000000"}}"#;
        let chunk: ResponseChunk =
            serde_json::from_str(legacy).expect("legacy payload without origin must deserialize");
        match chunk {
            ResponseChunk::ToolApprovalRequest { origin, tool, .. } => {
                assert_eq!(tool, "exec");
                assert_eq!(origin, None, "absent origin must default to None, not error");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// M8: a payload WITH `origin` round-trips faithfully — proves the field is a
    /// real, serialized part of the envelope (not silently dropped) for consumers
    /// that render the "who is asking" line.
    #[test]
    fn origin_roundtrips() {
        let chunk = ResponseChunk::ToolApprovalRequest {
            tool: "odoo_create".to_string(),
            args: "{}".to_string(),
            approval_id: Uuid::nil(),
            origin: Some("L1:developer".to_string()),
            reversible: false,
        };
        let json = serde_json::to_string(&chunk).expect("serialize");
        assert!(json.contains("\"origin\":\"L1:developer\""));

        let round_tripped: ResponseChunk = serde_json::from_str(&json).expect("deserialize");
        match round_tripped {
            ResponseChunk::ToolApprovalRequest { origin, .. } => {
                assert_eq!(origin, Some("L1:developer".to_string()));
            }
            other => panic!("unexpected variant after roundtrip: {other:?}"),
        }
    }

    /// M8: `reversible` is `bool` + `#[serde(default)]` — a pre-phase-3 payload with
    /// no `reversible` key must still deserialize (defaulting to `false`, the safe
    /// "treat as final" reading) rather than error.
    #[test]
    fn reversible_absent_payload_deserializes_to_false() {
        let legacy = r#"{"type":"ToolApprovalRequest","data":{"tool":"exec","args":"{}","approval_id":"00000000-0000-0000-0000-000000000000"}}"#;
        let chunk: ResponseChunk = serde_json::from_str(legacy)
            .expect("legacy payload without reversible must deserialize");
        match chunk {
            ResponseChunk::ToolApprovalRequest { reversible, .. } => {
                assert!(!reversible, "absent reversible must default to false");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// A `reversible: true` prompt (M2 cap-escalated delete) round-trips faithfully —
    /// the UI badge distinction depends on this surviving serialization.
    #[test]
    fn reversible_true_roundtrips() {
        let chunk = ResponseChunk::ToolApprovalRequest {
            tool: "task_delete".to_string(),
            args: "{}".to_string(),
            approval_id: Uuid::nil(),
            origin: Some("L0".to_string()),
            reversible: true,
        };
        let json = serde_json::to_string(&chunk).expect("serialize");
        assert!(json.contains("\"reversible\":true"));

        let round_tripped: ResponseChunk = serde_json::from_str(&json).expect("deserialize");
        match round_tripped {
            ResponseChunk::ToolApprovalRequest { reversible, .. } => {
                assert!(reversible);
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

    /// M8/M4 (Harness Completion phase 3): a `ToolResult` payload minted BEFORE
    /// `reversible`/`journal_id` existed (neither key present) must still
    /// deserialize, defaulting `reversible` to `false` and `journal_id` to `None` —
    /// the guarantee that an old/in-flight chunk (or a Telegram/CLI adapter that
    /// never learns about the new fields) does not break after upgrade.
    #[test]
    fn tool_result_legacy_payload_without_new_fields_deserializes() {
        let legacy = r#"{"type":"ToolResult","data":{"name":"task_delete","ok":true}}"#;
        let chunk: ResponseChunk =
            serde_json::from_str(legacy).expect("legacy ToolResult payload must deserialize");
        match chunk {
            ResponseChunk::ToolResult {
                name,
                ok,
                reversible,
                journal_id,
            } => {
                assert_eq!(name, "task_delete");
                assert!(ok);
                assert!(!reversible, "absent reversible must default to false");
                assert_eq!(journal_id, None, "absent journal_id must default to None");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// A `ToolResult` WITH the new fields round-trips faithfully — proves they are
    /// real, serialized parts of the envelope for the GUI's inline-[Undo] affordance.
    #[test]
    fn tool_result_with_new_fields_roundtrips() {
        let chunk = ResponseChunk::ToolResult {
            name: "task_delete".to_string(),
            ok: true,
            reversible: true,
            journal_id: Some("journal-row-id".to_string()),
        };
        let json = serde_json::to_string(&chunk).expect("serialize");
        assert!(json.contains("\"reversible\":true"));
        assert!(json.contains("\"journal_id\":\"journal-row-id\""));

        let round_tripped: ResponseChunk = serde_json::from_str(&json).expect("deserialize");
        match round_tripped {
            ResponseChunk::ToolResult {
                reversible,
                journal_id,
                ..
            } => {
                assert!(reversible);
                assert_eq!(journal_id, Some("journal-row-id".to_string()));
            }
            other => panic!("unexpected variant after roundtrip: {other:?}"),
        }
    }

    /// An irreversible/read call reports `reversible:false, journal_id:None` and
    /// still round-trips distinctly from a reversible one — guards against the two
    /// shapes being conflated by a lazy `Default` derive somewhere downstream.
    #[test]
    fn tool_result_irreversible_shape_has_no_journal_id() {
        let chunk = ResponseChunk::ToolResult {
            name: "web_search".to_string(),
            ok: true,
            reversible: false,
            journal_id: None,
        };
        let json = serde_json::to_string(&chunk).expect("serialize");
        let round_tripped: ResponseChunk = serde_json::from_str(&json).expect("deserialize");
        match round_tripped {
            ResponseChunk::ToolResult {
                reversible,
                journal_id,
                ..
            } => {
                assert!(!reversible);
                assert_eq!(journal_id, None);
            }
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
