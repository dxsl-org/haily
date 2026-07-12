//! ACP semantic layer (Sub-Agent + Skill Architecture phase 12) — the pure mapping between
//! Haily's existing pipeline surface and the ACP wire vocabulary. No I/O, no engine logic:
//! this is a rendering + gating translation only, so every function here is unit-testable
//! without a socket, a DB, or a live editor.

use crate::{Request, RequestOrigin, RunEvent, TranscriptEntry};
use serde_json::{json, Value};
use std::path::Path;
use uuid::Uuid;

// ---- ACP method names (the flat dialect this channel speaks) --------------------------

pub const M_INITIALIZE: &str = "initialize";
pub const M_SESSION_NEW: &str = "session/new";
pub const M_SESSION_LOAD: &str = "session/load";
pub const M_SESSION_PROMPT: &str = "session/prompt";
pub const M_SESSION_CANCEL: &str = "session/cancel";
pub const M_SESSION_SET_MODE: &str = "session/set_mode";
/// Notification FROM agent → client: streamed session updates.
pub const M_SESSION_UPDATE: &str = "session/update";
/// Request FROM agent → client: a permission prompt.
pub const M_REQUEST_PERMISSION: &str = "session/request_permission";

/// ACP protocol version this agent advertises. A bare integer keeps the hand-rolled dialect
/// forward-simple; a client negotiating a different major version is answered with our own.
pub const PROTOCOL_VERSION: u64 = 1;

// ---- Session modes → auto-approve policy ----------------------------------------------

/// The ACP session mode, mapped onto Haily's existing auto-approve behavior. `Default`
/// prompts for everything the broker would prompt for; `AcceptEdits` silently allows a
/// reversible (journaled/undoable) edit but still prompts for an irreversible action;
/// `DontAsk` allows everything the broker asks about. A sensitive-path write is prompted
/// UNCONDITIONALLY in every mode — see [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionMode {
    #[default]
    Default,
    AcceptEdits,
    DontAsk,
}

impl SessionMode {
    /// Lenient parse of an ACP `modeId` string. Anything unrecognized falls back to the
    /// safest mode (`Default` = prompt for everything), so a typo can never silently widen
    /// auto-approval — mirrors `DepthMode::from_label`'s "unknown → safe default" rule.
    pub fn from_id(s: &str) -> SessionMode {
        match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "accept_edits" | "acceptedits" | "accept" => SessionMode::AcceptEdits,
            "dont_ask" | "dontask" | "yolo" | "bypass_permissions" => SessionMode::DontAsk,
            _ => SessionMode::Default,
        }
    }

    pub fn as_id(self) -> &'static str {
        match self {
            SessionMode::Default => "default",
            SessionMode::AcceptEdits => "accept_edits",
            SessionMode::DontAsk => "dont_ask",
        }
    }
}

/// Whether a pending approval should be shown to the editor or auto-resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Send `session/request_permission` and wait for the human.
    Prompt,
    /// Auto-approve without prompting (session-mode policy).
    AutoAllow,
}

/// The permission-bridge policy. `sensitive` (a secret-path write) forces a prompt in EVERY
/// mode — the one hard invariant the plan calls out. Otherwise the mode decides: `AcceptEdits`
/// auto-allows only a `reversible` (undoable) edit; a genuinely irreversible action still
/// prompts. `DontAsk` auto-allows any non-sensitive action. `reversible` is read straight off
/// the `ResponseChunk::ToolApprovalRequest` — the adapter never re-derives tier logic.
pub fn decide(mode: SessionMode, reversible: bool, sensitive: bool) -> PermissionDecision {
    if sensitive {
        return PermissionDecision::Prompt; // unconditional, even in AcceptEdits/DontAsk
    }
    match mode {
        SessionMode::Default => PermissionDecision::Prompt,
        SessionMode::AcceptEdits => {
            if reversible {
                PermissionDecision::AutoAllow
            } else {
                PermissionDecision::Prompt
            }
        }
        SessionMode::DontAsk => PermissionDecision::AutoAllow,
    }
}

/// Secret deny-glob mirror of `haily_tools::coding::path_guard::is_secret_path`. Kept LOCAL
/// (not shared) for the same reason `run_event::strip_tool_tags` is a local 4th copy:
/// `haily-tools` sits ABOVE `haily-io` in the layering, so it cannot be imported here, and
/// the rule is a dozen self-contained lines. This must stay in sync with the canonical copy;
/// it is the guard that keeps `AcceptEdits`/`DontAsk` from ever silently writing a credential.
pub fn is_sensitive_path(rel: &str) -> bool {
    let lower = rel.replace('\\', "/").to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(lower.as_str());
    name.starts_with(".env")
        || name.ends_with(".pem")
        || name.starts_with("id_rsa")
        || lower == ".git/config"
        || lower.ends_with("/.git/config")
        || name.contains("secret")
        || name.contains("token")
        || name.contains("credential")
}

/// Best-effort extraction of the target file path from a tool-approval `args` JSON blob, so
/// the sensitive-path check and the edit-diff preview can find it. Recognizes the common
/// coding-write shapes (`path`/`file`/`file_path`). `None` for a non-file tool (e.g. a
/// connector call) — which simply means "no edit-diff / no path-based sensitivity".
pub fn extract_write_path(args: &str) -> Option<String> {
    let v: Value = serde_json::from_str(args).ok()?;
    for key in ["path", "file", "file_path", "filename"] {
        if let Some(p) = v.get(key).and_then(Value::as_str) {
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// Build an ACP `kind:"edit"` diff content block: read the CURRENT on-disk text as `oldText`
/// and pair it with the proposed `newText`. This is the compute-before-execute preview the
/// editor renders inline while the write is BLOCKED on the permission response. A path that
/// does not yet exist yields an empty `oldText` (a new-file creation), never an error.
pub fn compute_edit_diff(abs_path: &Path, new_text: &str) -> Value {
    let old_text = std::fs::read_to_string(abs_path).unwrap_or_default();
    json!({
        "kind": "edit",
        "path": abs_path.to_string_lossy(),
        "oldText": old_text,
        "newText": new_text,
    })
}

/// The four permission options offered to the editor, in ACP's `optionId`/`name`/`kind`
/// vocabulary. `allow_once`/`reject_once` resolve just this call; `allow_always` additionally
/// switches the session into `AcceptEdits`; `reject_always` denies this call.
pub fn permission_options() -> Value {
    json!([
        { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
        { "optionId": "allow_always", "name": "Allow and accept edits this session", "kind": "allow_always" },
        { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" },
        { "optionId": "reject_always", "name": "Reject always", "kind": "reject_always" },
    ])
}

/// Interpret the client's `session/request_permission` RESULT into `(approved, mode_update)`.
/// FAIL-SAFE: anything that is not an explicit allow — a reject, a `cancelled` outcome, an
/// unknown option, or a malformed result — is a DENY. `allow_always` also carries a switch
/// to `AcceptEdits` for the rest of the session.
pub fn interpret_permission_outcome(result: &Value) -> (bool, Option<SessionMode>) {
    let outcome = &result["outcome"];
    // ACP shape: { outcome: { outcome: "selected"|"cancelled", optionId?: "..." } }
    match outcome["outcome"].as_str() {
        Some("selected") => match outcome["optionId"].as_str() {
            Some("allow_once") => (true, None),
            Some("allow_always") => (true, Some(SessionMode::AcceptEdits)),
            _ => (false, None), // reject_once / reject_always / unknown → deny
        },
        // "cancelled" or anything else → deny.
        _ => (false, None),
    }
}

/// Map one ordered [`RunEvent`] to a `session/update` params object. Every variant has a
/// surface (milestones become `agent_message`/`tool_call` updates; a streamed chunk becomes
/// an `agent_message_chunk`); `DiffAvailable` becomes a `tool_call` update flagged as a diff
/// so an editor can offer the review. Content is already tag-stripped at the delivery
/// chokepoint, so it is rendered as inert data here.
pub fn run_event_update(session_id: &str, event: &RunEvent) -> Value {
    let update = match event {
        RunEvent::RunStarted { run_id, .. } => json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": format!("▶ run {run_id} started") }
        }),
        RunEvent::StageStarted { stage, tier, .. } => {
            let t = tier.as_deref().map(|t| format!(" ({t})")).unwrap_or_default();
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("── stage: {stage}{t}") } })
        }
        RunEvent::StageOutput { chunk, .. } => json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": chunk }
        }),
        RunEvent::GateResult { gate, pass, decisive, .. } => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": format!("gate:{gate}"),
            "status": if *pass { "completed" } else { "failed" },
            "content": [{ "type": "content", "content": { "type": "text", "text": decisive } }]
        }),
        RunEvent::Retry { attempt, .. } => json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("retry: attempt {attempt}") }
        }),
        RunEvent::Escalation { from, to, .. } => json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("escalated {from} → {to}") }
        }),
        RunEvent::DiffAvailable { file, .. } => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": format!("diff:{file}"),
            "status": "completed",
            "content": [{ "type": "diff", "path": file }]
        }),
        RunEvent::ApprovalNeeded { approval_id, .. } => json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("approval needed ({approval_id})") }
        }),
        RunEvent::PlanReady { plan_path, .. } => json!({
            "sessionUpdate": "plan",
            "plan": { "path": plan_path }
        }),
        RunEvent::RunPaused { reason, .. } => json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("run paused: {reason}") }
        }),
        RunEvent::RunComplete { outcome, .. } => json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("run complete: {outcome}") }
        }),
    };
    with_session_id(session_id, update)
}

/// A plain streamed-text `session/update` (used to relay `ResponseChunk::Text` and to replay
/// a transcript entry).
pub fn text_update(session_id: &str, role: &str, text: &str) -> Value {
    with_session_id(
        session_id,
        json!({
            "sessionUpdate": if role == "user" { "user_message_chunk" } else { "agent_message_chunk" },
            "content": { "type": "text", "text": text }
        }),
    )
}

/// Replay params for `session/load`: one `session/update` per transcript entry, oldest first.
pub fn transcript_updates(session_id: &str, entries: &[TranscriptEntry]) -> Vec<Value> {
    entries
        .iter()
        .map(|e| text_update(session_id, &e.role, &e.content))
        .collect()
}

fn with_session_id(session_id: &str, mut update: Value) -> Value {
    if let Value::Object(map) = &mut update {
        json!({ "sessionId": session_id, "update": map })
    } else {
        json!({ "sessionId": session_id, "update": update })
    }
}

/// The `initialize` result: advertise the capabilities the plan requires (load_session,
/// fork/list/resume, image prompt input) plus the available session modes.
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": { "image": true, "embeddedContext": true },
            "sessionCapabilities": { "fork": true, "list": true, "resume": true }
        },
        "authMethods": []
    })
}

/// The `session/new` (or load) result: the stable public session id plus the mode menu tied
/// to the P3 tiers' auto-approve policy.
pub fn session_result(acp_session_id: &str, mode: SessionMode) -> Value {
    json!({
        "sessionId": acp_session_id,
        "modes": {
            "currentModeId": mode.as_id(),
            "availableModes": [
                { "id": "default", "name": "Default (ask every time)" },
                { "id": "accept_edits", "name": "Accept edits" },
                { "id": "dont_ask", "name": "Don't ask" }
            ]
        }
    })
}

/// The `session/request_permission` params for a pending tool approval.
pub fn permission_request_params(session_id: &str, tool: &str, args: &str, edit_diff: Option<Value>) -> Value {
    let mut content = vec![json!({ "type": "content", "content": { "type": "text", "text": args } })];
    if let Some(diff) = edit_diff {
        content.push(json!({ "type": "content", "content": diff }));
    }
    json!({
        "sessionId": session_id,
        "toolCall": { "toolCallId": tool, "title": tool, "kind": "edit", "content": content },
        "options": permission_options()
    })
}

/// Build the orchestrator `Request` for a prompt arriving over ACP. ACP is a **Chat-class**
/// transport (SEC-H, phase 9): `origin` is left `RequestOrigin::Chat` — the eval-mode
/// plan-gate bypass is reachable ONLY from a direct CLI subcommand (`RequestOrigin::Cli`),
/// which this adapter can never mint. `#[serde(skip)]` on `Request::origin` means even a
/// crafted prompt payload cannot inject `Cli`.
pub fn build_prompt_request(session_id: Uuid, message: String, user_ref: Option<String>) -> Request {
    Request {
        session_id,
        adapter_id: super::ADAPTER_ID.to_string(),
        message,
        user_ref,
        depth: Default::default(),
        origin: RequestOrigin::Chat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_prompts_for_everything() {
        assert_eq!(decide(SessionMode::Default, true, false), PermissionDecision::Prompt);
        assert_eq!(decide(SessionMode::Default, false, false), PermissionDecision::Prompt);
    }

    #[test]
    fn accept_edits_auto_allows_reversible_but_prompts_irreversible() {
        assert_eq!(decide(SessionMode::AcceptEdits, true, false), PermissionDecision::AutoAllow);
        assert_eq!(decide(SessionMode::AcceptEdits, false, false), PermissionDecision::Prompt);
    }

    #[test]
    fn dont_ask_auto_allows_non_sensitive() {
        assert_eq!(decide(SessionMode::DontAsk, true, false), PermissionDecision::AutoAllow);
        assert_eq!(decide(SessionMode::DontAsk, false, false), PermissionDecision::AutoAllow);
    }

    /// The load-bearing invariant: a sensitive-path write is prompted in EVERY mode, even the
    /// most permissive — so `DontAsk`/`AcceptEdits` can never silently overwrite a credential.
    #[test]
    fn sensitive_path_prompts_unconditionally_in_every_mode() {
        for mode in [SessionMode::Default, SessionMode::AcceptEdits, SessionMode::DontAsk] {
            assert_eq!(
                decide(mode, true, true),
                PermissionDecision::Prompt,
                "sensitive write must prompt even in {mode:?}"
            );
        }
    }

    #[test]
    fn is_sensitive_path_matches_the_secret_deny_glob() {
        assert!(is_sensitive_path(".env"));
        assert!(is_sensitive_path("config/app_secret.json"));
        assert!(is_sensitive_path("deploy/id_rsa"));
        assert!(is_sensitive_path("keys/server.PEM"));
        assert!(is_sensitive_path(".git/config"));
        assert!(!is_sensitive_path("src/main.rs"));
    }

    #[test]
    fn extract_write_path_reads_common_shapes() {
        assert_eq!(extract_write_path(r#"{"path":"src/a.rs","content":"x"}"#), Some("src/a.rs".into()));
        assert_eq!(extract_write_path(r#"{"file_path":".env"}"#), Some(".env".into()));
        assert_eq!(extract_write_path(r#"{"query":"select"}"#), None);
        assert_eq!(extract_write_path("not json"), None);
    }

    /// compute-before-execute: `oldText` is read from disk, `newText` is the proposal — the
    /// exact contract the "edit-diff produces correct old/new text" success criterion asserts.
    #[test]
    fn compute_edit_diff_reads_old_from_disk_and_pairs_new() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn old() {}").unwrap();
        let block = compute_edit_diff(&file, "fn new() {}");
        assert_eq!(block["kind"], "edit");
        assert_eq!(block["oldText"], "fn old() {}");
        assert_eq!(block["newText"], "fn new() {}");
    }

    #[test]
    fn compute_edit_diff_new_file_has_empty_old_text() {
        let dir = tempfile::tempdir().unwrap();
        let block = compute_edit_diff(&dir.path().join("does_not_exist.rs"), "created");
        assert_eq!(block["oldText"], "");
        assert_eq!(block["newText"], "created");
    }

    #[test]
    fn permission_outcome_only_allow_is_approved_everything_else_denies() {
        let allow = json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } });
        assert_eq!(interpret_permission_outcome(&allow), (true, None));

        let allow_always = json!({ "outcome": { "outcome": "selected", "optionId": "allow_always" } });
        assert_eq!(interpret_permission_outcome(&allow_always), (true, Some(SessionMode::AcceptEdits)));

        let reject = json!({ "outcome": { "outcome": "selected", "optionId": "reject_once" } });
        assert_eq!(interpret_permission_outcome(&reject), (false, None));

        let cancelled = json!({ "outcome": { "outcome": "cancelled" } });
        assert_eq!(interpret_permission_outcome(&cancelled), (false, None));

        // Malformed / missing → deny (fail-safe).
        assert_eq!(interpret_permission_outcome(&json!({})), (false, None));
    }

    #[test]
    fn mode_parse_is_lenient_and_unknown_is_safe_default() {
        assert_eq!(SessionMode::from_id("accept_edits"), SessionMode::AcceptEdits);
        assert_eq!(SessionMode::from_id("dont-ask"), SessionMode::DontAsk);
        assert_eq!(SessionMode::from_id("garbage"), SessionMode::Default);
    }

    #[test]
    fn transcript_updates_preserve_order_and_role() {
        let entries = vec![
            TranscriptEntry { role: "user".into(), content: "hi".into() },
            TranscriptEntry { role: "assistant".into(), content: "hello".into() },
        ];
        let ups = transcript_updates("s1", &entries);
        assert_eq!(ups.len(), 2);
        assert_eq!(ups[0]["update"]["sessionUpdate"], "user_message_chunk");
        assert_eq!(ups[0]["update"]["content"]["text"], "hi");
        assert_eq!(ups[1]["update"]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(ups[1]["sessionId"], "s1");
    }

    /// SEC-H (phase 9): an ACP prompt is Chat-class and can NEVER be Cli-origin, so it can
    /// never reach eval mode's privileged plan-gate bypass. Reuses the same structural check
    /// the eval runner relies on — origin is a transport marker, not derivable from content.
    #[test]
    fn acp_prompt_request_is_chat_origin_never_cli() {
        let req = build_prompt_request(Uuid::new_v4(), "make a change".into(), None);
        assert_eq!(req.origin, RequestOrigin::Chat, "ACP is a Chat-class transport, never Cli");
        assert_eq!(req.adapter_id, super::super::ADAPTER_ID);
    }

    #[test]
    fn run_event_update_carries_session_id_and_maps_stage_output() {
        let up = run_event_update("s7", &RunEvent::StageOutput { run_id: "r".into(), seq: 0, chunk: "compiling".into() });
        assert_eq!(up["sessionId"], "s7");
        assert_eq!(up["update"]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(up["update"]["content"]["text"], "compiling");
    }
}
