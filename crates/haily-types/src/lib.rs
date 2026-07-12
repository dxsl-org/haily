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

/// Mobile thin-client wire protocol — envelope, frame catalogue, pairing DTOs (Mobile
/// Thin-Client plan phase 1). See `docs/mobile-protocol.md` for the full spec these types
/// implement; that document and this module are the same contract, kept in sync in the same
/// PR as a rule (a serde change here IS a spec edit there).
pub mod mobile;

pub use mobile::{
    ClientFrame, MobileApprovalPolicy, MobileError, PairRequest, PairResponse, PairingQr,
    ServerBody, ServerFrame, SessionSnapshot, PROTOCOL_VERSION,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub session_id: Uuid,
    pub adapter_id: String,
    pub message: String,
    pub user_ref: Option<String>,
    /// Judgment-depth requested for this turn (Sub-Agent + Skill Architecture phase 7).
    /// Set from a GUI toggle OR a VN/EN depth phrase detected in `message` (never from
    /// tool/pasted content) — `Deep` buys multi-stream judgment (judge panel, refuter
    /// votes, apex judge) at explicit 3–5× cost. `#[serde(default)]` keeps this ADDITIVE:
    /// a payload minted before the field existed deserializes to `DepthMode::Normal`, so
    /// no adapter is forced to set it and the wire envelope stays backward-compatible.
    #[serde(default)]
    pub depth: DepthMode,
    /// Transport that produced this request (Sub-Agent + Skill Architecture phase 9, SEC-H).
    /// Every I/O adapter (GUI, interactive CLI REPL, Telegram) is a [`RequestOrigin::Chat`]
    /// transport routing a user message through the orchestrator; [`RequestOrigin::Cli`] is
    /// reserved for a direct CLI SUBCOMMAND invocation (`haily eval …`) and is the ONLY origin
    /// permitted to enable eval-mode's privileged plan-gate bypass + ship hard-block.
    ///
    /// `#[serde(skip)]` (NOT just `default`) is LOAD-BEARING: origin is an in-process transport
    /// marker that must NEVER cross a serialization boundary — any Request deserialized from a
    /// wire/GUI/persisted payload always yields the default [`RequestOrigin::Chat`], so a remote
    /// or chat payload can never inject `Cli`. Only in-process direct construction (the eval CLI
    /// entrypoint) sets `Cli`; every adapter leaves it `Chat`.
    #[serde(skip)]
    pub origin: RequestOrigin,
}

/// Request transport origin — the SEC-H structural gate for eval mode (phase 9). See
/// [`Request::origin`]. Defined in the leaf `haily-types` crate so `Request` can carry it typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RequestOrigin {
    /// A user message from an I/O adapter (GUI, CLI REPL, Telegram) — the default. Can never
    /// enable eval mode.
    #[default]
    Chat,
    /// A direct CLI subcommand invocation (`haily eval …`). The ONLY origin eval mode accepts.
    Cli,
}

/// Per-request judgment depth. `Deep` is NEVER auto-selected — it is set only by an
/// explicit user action (GUI toggle or a genuine user-message phrase); the harness never
/// escalates to it on its own (phase 7 LOCKED decision). Defined HERE in the leaf
/// `haily-types` crate — like [`RunEvent`] — so [`Request`] can carry it typed without
/// `haily-types` depending on `haily-core` (where the phrase mapper + judge machinery
/// live). `haily-core::depth` re-exports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DepthMode {
    /// Trim stages: skip parallel scout + red-team (plan) and refuter votes (build).
    Quick,
    /// The default balanced pipeline.
    #[default]
    Normal,
    /// Add the judge panel (plan Design), refuter votes (build review), and apex-judge
    /// adjudication at explicit 3–5× cost.
    Deep,
}

impl DepthMode {
    /// Lenient parse of a wire/label string (a GUI toggle value or the phrase-mapper's
    /// output). An unrecognized value falls back to [`DepthMode::Normal`] — NEVER
    /// [`DepthMode::Deep`], so a typo can never silently escalate cost (phase 7: Deep is
    /// only ever reached by an exact, explicit match).
    pub fn from_label(s: &str) -> DepthMode {
        match s.trim().to_lowercase().as_str() {
            "quick" => DepthMode::Quick,
            "deep" => DepthMode::Deep,
            _ => DepthMode::Normal,
        }
    }

    /// The canonical lowercase label (matches the serde wire form).
    pub fn as_label(self) -> &'static str {
        match self {
            DepthMode::Quick => "quick",
            DepthMode::Normal => "normal",
            DepthMode::Deep => "deep",
        }
    }
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
        /// merits (e.g. `calendar_delete`, `worktree_apply`). Display-only — lets a UI
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
    /// A learning-loop distillation PROPOSAL surfaced for user approval (Sub-Agent + Skill
    /// Architecture phase 8, DEP-C2). Emitted from `haily-core` when the pipeline's recurrence
    /// detector finds ≥2 same-class review findings across runs; carries only the rendered,
    /// already-tag-stripped proposal text — NEVER a silent write to standards. Approval (a
    /// separate, explicit user action) is what appends it to the out-of-workspace standards
    /// overlay (SEC-H). Crosses core→io ONLY as this leaf-type variant over the existing mpsc,
    /// mapped to a [`ProactiveCardKind::DistillationProposal`] card by [`ProactiveCard::from_notification`]
    /// — `haily-core` never imports `haily-io` (CLAUDE.md layering invariant).
    DistillationProposal {
        /// `category:module` class key this proposal addresses — also the dedup/cooldown key
        /// so a dismissed proposal does not re-fire for the same class within the cooldown.
        class_key: String,
        /// The rendered, itemized, tag-stripped proposal text shown to the user.
        summary: String,
        /// Number of distilled rules in the proposal.
        rule_count: u32,
    },
    /// The `safety.disable_writes` kill switch changed state (Mobile Thin-Client plan
    /// phase 2a, red team m7/M15). The kill switch is intentionally GLOBAL — flipping it
    /// from ANY frontend (desktop GUI, Telegram, CLI, mobile) must be reflected on every
    /// OTHER frontend, so every channel's displayed state stays consistent with the one
    /// shared underlying safety property. Broadcast via `notify_all`, same as every other
    /// `Notification` variant — no separate watch channel needed.
    KillStateChanged {
        on: bool,
    },
}

/// Typed, ordered observability stream for a pipeline RUN (Sub-Agent + Skill Architecture
/// phase 4). The runner (phase 4b) is the single source of truth for run state, so it emits
/// this sequence; defined HERE in the leaf `haily-types` crate so `haily-core` can emit it
/// without importing `haily-io` (the "core never imports io" invariant).
///
/// This is the CONTRACT + type only. The ordered delivery channel and per-channel rendering
/// (GUI timeline, ACP `session/update`, Telegram pings, TUI progress) are built in P11/P12 —
/// no delivery is wired here.
///
/// Follows the ResponseChunk additive convention: `#[serde(tag="type", content="data")]` with
/// `#[serde(default)]` on any field that may be absent in an older payload, so the wire
/// envelope stays backward-compatible as variants gain fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "data")]
pub enum RunEvent {
    /// A run began. `work_item_id` is the owning long-running item.
    RunStarted { run_id: String, work_item_id: String },
    /// A stage began executing. `tier` is the resolved model tier NAME (e.g. `"thinking"`),
    /// a display string rather than the `haily-llm::Tier` type — `haily-types` is a leaf and
    /// must not depend on `haily-llm`. `#[serde(default)]` keeps it additive: a stage with no
    /// tier override, or a pre-`tier` payload, deserializes to `None`.
    StageStarted {
        run_id: String,
        stage: String,
        #[serde(default)]
        tier: Option<String>,
    },
    /// A chunk of a stage's streamed output. `seq` is the per-run monotonic sequence number so
    /// a consumer can order/dedupe chunks; `chunk` is the (tag-stripped) text.
    StageOutput { run_id: String, seq: u64, chunk: String },
    /// A gate finished. `gate` is the gate KIND label (`"command"`/`"artifact"`/`"approval"`,
    /// never a path or command), `pass` is the verdict, `decisive` is the shortest decisive
    /// output (empty on pass) — already rendered as inert data by the verifier parser.
    GateResult { run_id: String, gate: String, pass: bool, decisive: String },
    /// A verifier-grounded retry of the current stage began. `attempt` is the new 0-based count.
    Retry { run_id: String, attempt: u32 },
    /// The current stage escalated its model tier. `from`/`to` are tier NAME strings.
    Escalation { run_id: String, from: String, to: String },
    /// A diff is available for review. `file` is the changed path (repo-relative).
    DiffAvailable { run_id: String, file: String },
    /// A stage's approval gate needs the user. `approval_id` is the broker's approval id.
    ApprovalNeeded { run_id: String, approval_id: String },
    /// A plan artifact is ready for review. `plan_path` is the produced plan file.
    PlanReady { run_id: String, plan_path: String },
    /// The run paused (retries exhausted, approval wait, or explicit stop). `reason` is a short
    /// human-facing cause.
    RunPaused { run_id: String, reason: String },
    /// The run reached a terminal state. `outcome` is the terminal RunStatus name
    /// (`"done"`/`"failed"`) or a short outcome label.
    RunComplete { run_id: String, outcome: String },
}

/// A single discrete proactive event, shaped for a dedicated display surface (the
/// GUI's card panel — phase 08) rather than the raw daemon-wide `Notification`
/// broadcast. Deliberately a SEPARATE type rather than re-wrapping `Notification`
/// directly: `Notification::WorkItemsChanged` is a full-snapshot concern with its own
/// channel/panel (phase 5) and does not belong on a "discrete event" surface, and
/// keeping this enum closed to the other three variants stops that surface from
/// silently growing new unrelated cases underneath it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProactiveCard {
    /// Synthetic per-event id — a stable list key for the frontend, NOT a DB row id
    /// (a `ReminderFired` card's `reminder_id` is the DB-backed id, when one exists).
    pub id: Uuid,
    /// RFC3339 generation time, for display only ("fired at HH:MM"). Ordering of
    /// cards for eviction/rendering purposes comes from `Vec` insertion order, not
    /// this field — see `ProactiveCard::from_notification`'s callers.
    pub created_at: String,
    pub kind: ProactiveCardKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ProactiveCardKind {
    MorningBrief { text: String },
    Alert { title: String, body: String, urgent: bool },
    ReminderFired { reminder_id: Uuid, title: String },
    /// A learning-loop distillation proposal awaiting user approval (phase 8) — the card
    /// surface of [`Notification::DistillationProposal`]. Display + approve/dismiss only; the
    /// approve action (wired at the app/GUI layer) is the sole path that writes the overlay.
    DistillationProposal { class_key: String, summary: String, rule_count: u32 },
}

impl ProactiveCard {
    /// Builds a card from a `Notification`. Returns `None` for `WorkItemsChanged` —
    /// that variant is forwarded over its own dedicated channel (see
    /// `haily_io::gui::GuiWorkItemsReceiver`) and has no card representation; callers
    /// must treat `None` as "nothing to forward on this surface", not an error.
    pub fn from_notification(msg: &Notification) -> Option<Self> {
        let kind = match msg {
            Notification::MorningBrief(text) => ProactiveCardKind::MorningBrief { text: text.clone() },
            Notification::Alert { title, body, urgent } => ProactiveCardKind::Alert {
                title: title.clone(),
                body: body.clone(),
                urgent: *urgent,
            },
            Notification::ReminderFired { reminder_id, title } => ProactiveCardKind::ReminderFired {
                reminder_id: *reminder_id,
                title: title.clone(),
            },
            Notification::DistillationProposal {
                class_key,
                summary,
                rule_count,
            } => ProactiveCardKind::DistillationProposal {
                class_key: class_key.clone(),
                summary: summary.clone(),
                rule_count: *rule_count,
            },
            Notification::WorkItemsChanged(_) => return None,
            // Live safety-state signal, not a discrete/dismissable event card — a frontend
            // that cares (GUI toggle, mobile hello-ack) reads it directly off this variant
            // rather than through the card surface, mirroring `WorkItemsChanged`.
            Notification::KillStateChanged { .. } => return None,
        };
        Some(ProactiveCard {
            id: Uuid::new_v4(),
            created_at: chrono::Utc::now().to_rfc3339(),
            kind,
        })
    }
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

/// A single persisted transcript entry for session replay (Sub-Agent + Skill Architecture
/// phase 12). `role` is `"user"`/`"assistant"` (matching `messages.role`); `content` is the
/// stored message text. `Serialize`/`Deserialize` (added for the mobile thin-client's
/// `SessionSnapshot` frame, phase 1) is purely additive — no existing caller serializes this
/// type today, so adding the derive changes no wire shape in production.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub role: String,
    pub content: String,
}

/// Read-only view of a session's persisted message history, for the ACP channel's
/// `session/load` transcript replay (phase 12). Lives in the leaf `haily-types` crate so a
/// `haily-io` adapter can replay a transcript without depending on `haily-db` (the CLAUDE.md
/// layering invariant) — the DB-backed implementation is constructed at the app layer and
/// injected post-construction, exactly like [`ApprovalResolver`]. A channel with no replay
/// surface never needs it (the [`crate` adapter hook][crate] defaults to no injection, which
/// yields an empty transcript rather than an error).
#[async_trait]
pub trait SessionTranscript: Send + Sync {
    /// Return the session's messages in chronological order (oldest first), or an empty
    /// vec for an unknown/empty session. MUST NOT error the caller — replay is best-effort
    /// UX, never a correctness gate, so an implementation that hits a DB error logs and
    /// returns what it has (possibly empty).
    async fn transcript(&self, session_id: &str) -> Vec<TranscriptEntry>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_mode_defaults_to_normal_and_is_lowercase_on_the_wire() {
        assert_eq!(DepthMode::default(), DepthMode::Normal);
        assert_eq!(serde_json::to_string(&DepthMode::Deep).unwrap(), "\"deep\"");
        assert_eq!(serde_json::to_string(&DepthMode::Quick).unwrap(), "\"quick\"");
        assert_eq!(
            serde_json::from_str::<DepthMode>("\"deep\"").unwrap(),
            DepthMode::Deep
        );
    }

    /// ADDITIVE guarantee: a `Request` payload minted before `depth` existed (no `depth`
    /// key) must still deserialize, defaulting to `Normal` — never error.
    #[test]
    fn request_without_depth_deserializes_to_normal() {
        let legacy = r#"{"session_id":"00000000-0000-0000-0000-000000000000","adapter_id":"cli","message":"hi","user_ref":null}"#;
        let req: Request = serde_json::from_str(legacy).expect("legacy Request must deserialize");
        assert_eq!(req.depth, DepthMode::Normal, "absent depth must default to Normal");
    }

    #[test]
    fn request_with_deep_depth_roundtrips() {
        let req = Request {
            session_id: Uuid::nil(),
            adapter_id: "gui".into(),
            message: "làm kỹ vào".into(),
            user_ref: None,
            depth: DepthMode::Deep,
            origin: RequestOrigin::Chat,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains("\"depth\":\"deep\""));
        let round: Request = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round.depth, DepthMode::Deep);
    }

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

    /// `WorkItemsChanged` has no card representation — this is the load-bearing
    /// guarantee `haily_io::gui::GuiAdapter::notify` relies on to know when NOT to
    /// touch the proactive-card watch channel.
    #[test]
    fn proactive_card_from_work_items_changed_is_none() {
        let msg = Notification::WorkItemsChanged(vec![]);
        assert!(ProactiveCard::from_notification(&msg).is_none());
    }

    /// Every non-`WorkItemsChanged` variant maps to a card carrying the same data,
    /// plus a freshly-minted id and timestamp.
    #[test]
    fn proactive_card_from_each_discrete_kind() {
        let brief = ProactiveCard::from_notification(&Notification::MorningBrief("hi".into()))
            .expect("MorningBrief must produce a card");
        assert!(matches!(brief.kind, ProactiveCardKind::MorningBrief { text } if text == "hi"));

        let alert = ProactiveCard::from_notification(&Notification::Alert {
            title: "t".into(),
            body: "b".into(),
            urgent: true,
        })
        .expect("Alert must produce a card");
        match alert.kind {
            ProactiveCardKind::Alert { title, body, urgent } => {
                assert_eq!(title, "t");
                assert_eq!(body, "b");
                assert!(urgent);
            }
            other => panic!("unexpected kind: {other:?}"),
        }

        let rid = Uuid::new_v4();
        let reminder = ProactiveCard::from_notification(&Notification::ReminderFired {
            reminder_id: rid,
            title: "call mom".into(),
        })
        .expect("ReminderFired must produce a card");
        match reminder.kind {
            ProactiveCardKind::ReminderFired { reminder_id, title } => {
                assert_eq!(reminder_id, rid);
                assert_eq!(title, "call mom");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    /// Wire shape sanity: the frontend's discriminated union (`src/lib/tauri.ts`)
    /// expects `{"type": "...", "data": {...}}` nested under the `kind` field —
    /// mirrors `ResponseChunk`'s existing `type`/`data` convention exactly.
    #[test]
    fn proactive_card_kind_serializes_with_type_and_data_tags() {
        let card = ProactiveCard {
            id: Uuid::nil(),
            created_at: "2026-07-07T00:00:00Z".into(),
            kind: ProactiveCardKind::Alert {
                title: "t".into(),
                body: "b".into(),
                urgent: false,
            },
        };
        let json = serde_json::to_string(&card).expect("serialize");
        assert!(json.contains("\"type\":\"Alert\""));
        assert!(json.contains("\"data\":"));
        let round_tripped: ProactiveCard = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(round_tripped.kind, ProactiveCardKind::Alert { urgent: false, .. }));
    }

    /// Phase 8 (DEP-C2): a `DistillationProposal` notification maps to a card carrying the
    /// same class key / summary / rule count — the proposal reaches the GUI card surface
    /// without `haily-core` importing `haily-io`.
    #[test]
    fn proactive_card_from_distillation_proposal() {
        let card = ProactiveCard::from_notification(&Notification::DistillationProposal {
            class_key: "critical:crates/haily-core".into(),
            summary: "1. Always handle the None case".into(),
            rule_count: 1,
        })
        .expect("DistillationProposal must produce a card");
        match card.kind {
            ProactiveCardKind::DistillationProposal { class_key, summary, rule_count } => {
                assert_eq!(class_key, "critical:crates/haily-core");
                assert_eq!(summary, "1. Always handle the None case");
                assert_eq!(rule_count, 1);
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    /// A `DistillationProposal` notification roundtrips through serde faithfully (additive
    /// enum variant — old payloads simply never carried it).
    #[test]
    fn distillation_proposal_notification_roundtrip() {
        let notif = Notification::DistillationProposal {
            class_key: "high:crates/haily-db".into(),
            summary: "1. Validate at the boundary".into(),
            rule_count: 2,
        };
        let json = serde_json::to_string(&notif).expect("serialize");
        let round: Notification = serde_json::from_str(&json).expect("deserialize");
        match round {
            Notification::DistillationProposal { class_key, rule_count, .. } => {
                assert_eq!(class_key, "high:crates/haily-db");
                assert_eq!(rule_count, 2);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// `KillStateChanged` is additive and has no card representation — mirrors
    /// `WorkItemsChanged`'s guarantee (a frontend that cares reads it directly).
    #[test]
    fn kill_state_changed_roundtrips_and_has_no_card() {
        let notif = Notification::KillStateChanged { on: true };
        let json = serde_json::to_string(&notif).expect("serialize");
        let round: Notification = serde_json::from_str(&json).expect("deserialize");
        match round {
            Notification::KillStateChanged { on } => assert!(on),
            other => panic!("unexpected variant: {other:?}"),
        }
        assert!(ProactiveCard::from_notification(&notif).is_none());
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

    #[test]
    fn run_event_uses_tag_content_envelope() {
        let ev = RunEvent::GateResult {
            run_id: "r1".to_string(),
            gate: "command".to_string(),
            pass: false,
            decisive: "verifier rust FAILED (exit 101)".to_string(),
        };
        let json = serde_json::to_string(&ev).expect("serialize");
        assert!(json.contains("\"type\":\"GateResult\""));
        assert!(json.contains("\"data\""));
        let round_tripped: RunEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, ev);
    }

    /// Additive convention: `StageStarted.tier` is `Option<String>` + `#[serde(default)]`, so a
    /// payload minted before `tier` existed (no `tier` key) must still deserialize, defaulting
    /// to `None` — the same guarantee the ResponseChunk `origin`/`reversible` tests assert, so a
    /// persisted or in-flight old RunEvent does not break after an upgrade adds fields.
    #[test]
    fn run_event_stage_started_tier_absent_payload_deserializes() {
        let legacy = r#"{"type":"StageStarted","data":{"run_id":"r1","stage":"plan"}}"#;
        let ev: RunEvent =
            serde_json::from_str(legacy).expect("legacy StageStarted without tier must deserialize");
        match ev {
            RunEvent::StageStarted { run_id, stage, tier } => {
                assert_eq!(run_id, "r1");
                assert_eq!(stage, "plan");
                assert_eq!(tier, None, "absent tier must default to None, not error");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn run_event_stage_started_tier_roundtrips() {
        let ev = RunEvent::StageStarted {
            run_id: "r1".to_string(),
            stage: "implement".to_string(),
            tier: Some("thinking".to_string()),
        };
        let json = serde_json::to_string(&ev).expect("serialize");
        assert!(json.contains("\"tier\":\"thinking\""));
        let round_tripped: RunEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, ev);
    }
}
