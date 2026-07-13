//! Tier decision core (Auto Model Routing R1, phase 3): [`select_tier`] picks the
//! [`haily_llm::Tier`] a turn/pipeline-stage should run on, purely from trusted-origin inputs.
//! Phase 4 wires this into `agent::turn` — [`select_tier`] feeds `current_tier` at the top of
//! `run_turn`, and [`open_stream_with_escalation`] is the escalation-aware replacement for the
//! turn loop's plain `complete_stream` call — and threads [`TierDecision::features`] into
//! `routing_decisions` (phase 2's log table).
//!
//! **Injection invariant (LOCKED):** every input this module reads is either the genuine user
//! message string or an already-derived trusted counter (`RouteCtx.history_user_msgs` — a
//! COUNT, never assembled history text). There is no field anywhere in [`RouteCtx`] for raw
//! tool-result/assistant text, so bloating the conversation's text content structurally cannot
//! change a decision — only the trusted counters can (`tier_intent`'s injection tests document
//! the reused source-guard; `injection_bloated_text_cannot_reach_routing` below documents the
//! API-shape half of the same guarantee).

use crate::depth::DepthMode;
use crate::tier_intent;
use haily_llm::{CompletionRequest, Egress, EscalationPolicy, LlmRouter, StreamChunk, Tier};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Self-calibrate from `routing_decisions` data once it exists (researcher-01: no published
/// thresholds exist for either constant) — these are deliberate placeholders, not tuned values.
const W_HIGH: usize = 80;
/// NOTE: self-calibrate from routing_decisions data (researcher-01: no published thresholds
/// exist). Phase file also refers to this as "H_CONT" in one place; "N_CONT" is the name used
/// throughout the decision ladder and is kept as the single canonical constant.
const N_CONT: usize = 6;

/// Derived, privacy-safe features behind a routing decision — mirrors the `feature_*` columns
/// `routing_decisions` (phase 2) persists. Never carries raw message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteFeatures {
    pub msg_words: usize,
    pub has_code: bool,
    pub history_user_msgs: usize,
    /// `DepthMode::as_label()` wire form ('quick' | 'normal' | 'deep').
    pub depth_label: &'static str,
}

impl RouteFeatures {
    fn extract(msg: &str, ctx: &RouteCtx) -> RouteFeatures {
        RouteFeatures {
            msg_words: crate::feedback_parser::word_count(msg.trim()),
            has_code: msg.contains("```"),
            history_user_msgs: ctx.history_user_msgs,
            depth_label: ctx.depth.as_label(),
        }
    }
}

/// Only meaningful as part of [`TierDecision::default`] — a routing-disabled turn never logs
/// these features (see the `routing_enabled=false ⇒ zero rows` identity contract in
/// `agent::turn`), so the exact zero values here are never observed, only the fact that
/// `tier`/`source` resolve to "session default, no decision made."
impl Default for RouteFeatures {
    fn default() -> Self {
        RouteFeatures { msg_words: 0, has_code: false, history_user_msgs: 0, depth_label: "normal" }
    }
}

/// Trusted context `select_tier` reads alongside the message. `history_user_msgs` is a COUNT
/// of prior user messages (never assembled history text — see the module-level injection
/// invariant); `remote_origin` mirrors the existing `deny_remote_deep` check (mobile/server.rs)
/// so the tier ceiling and the depth ceiling stay in lockstep for remote requests.
#[derive(Debug, Clone, Copy)]
pub struct RouteCtx {
    pub depth: DepthMode,
    pub history_user_msgs: usize,
    pub remote_origin: bool,
}

/// Matches the `routing_decisions.decision_source` column vocabulary 1:1 (haily-db
/// `queries/routing_decisions.rs`: `'default' | 'heuristic' | 'explicit_phrase' | 'depth'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    /// No rung of the ladder fired — the caller's session default tier stands.
    Default,
    Heuristic,
    ExplicitPhrase,
    Depth,
}

impl DecisionSource {
    pub fn as_label(self) -> &'static str {
        match self {
            DecisionSource::Default => "default",
            DecisionSource::Heuristic => "heuristic",
            DecisionSource::ExplicitPhrase => "explicit_phrase",
            DecisionSource::Depth => "depth",
        }
    }
}

/// The outcome of one `select_tier` call: `tier` is `None` when the session default should be
/// used untouched; `features` is exactly what the caller should log to `routing_decisions`.
#[derive(Debug, Clone)]
pub struct TierDecision {
    pub tier: Option<Tier>,
    pub source: DecisionSource,
    pub features: RouteFeatures,
}

/// The `routing_enabled=false` value — `agent::turn` uses this instead of calling
/// [`select_tier`] at all when the kill switch is off, so a disabled turn's tier is always
/// `None` (identical model selection to a pre-phase-4 turn) and its decision source is
/// unambiguously "no decision was made" rather than a fabricated heuristic result.
impl Default for TierDecision {
    fn default() -> Self {
        TierDecision { tier: None, source: DecisionSource::Default, features: RouteFeatures::default() }
    }
}

/// Wire label for a [`Tier`], matching the `routing_decisions.chosen_tier`/`escalated_to`
/// vocabulary. Lives here rather than as a `Tier` method because `haily-llm` is a leaf crate
/// with no wire-format/persistence concerns of its own.
pub fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "fast",
        Tier::Medium => "medium",
        Tier::Thinking => "thinking",
        Tier::Ultra => "ultra",
    }
}

fn depth_tier(depth: DepthMode) -> Option<Tier> {
    match depth {
        DepthMode::Deep => Some(Tier::Thinking),
        DepthMode::Quick => Some(Tier::Fast),
        DepthMode::Normal => None,
    }
}

/// One step down (never below `Fast`) — used by the `cost_quality` 0–3 bias.
fn step_down(tier: Tier) -> Tier {
    match tier {
        Tier::Fast => Tier::Fast,
        Tier::Medium => Tier::Fast,
        Tier::Thinking => Tier::Medium,
        Tier::Ultra => Tier::Thinking,
    }
}

/// One step up, capped at `Thinking` — the heuristic/knob path never reaches `Ultra`
/// (LOCKED: Ultra is explicit-phrase/pipeline only, never heuristic-reachable).
fn step_up_capped_thinking(tier: Tier) -> Tier {
    match tier {
        Tier::Fast => Tier::Medium,
        Tier::Medium => Tier::Thinking,
        Tier::Thinking | Tier::Ultra => Tier::Thinking,
    }
}

/// Applies the `cost_quality` 0–10 knob to a HEURISTIC-derived tier only (never to an explicit
/// phrase or `DepthMode` result — those are direct user requests the knob must not second-guess).
fn apply_cost_quality_bias(tier: Tier, cost_quality: u8) -> Tier {
    if cost_quality <= 3 {
        step_down(tier)
    } else if cost_quality >= 8 {
        step_up_capped_thinking(tier)
    } else {
        tier
    }
}

/// `msg_words > W_HIGH` or a code fence → `Medium`; the continuation guard floors a short
/// follow-up at `Medium` when the conversation already has substantial history — a curt
/// "ok fix that" mid-project should not drop back to `Fast`. Both branches converge on the
/// same base tier (`Medium`) before the `cost_quality` bias is applied.
fn heuristic_tier(features: &RouteFeatures, cost_quality: u8) -> Option<Tier> {
    let long_or_code = features.msg_words > W_HIGH || features.has_code;
    let continuation = features.msg_words <= crate::feedback_parser::SHORT_MESSAGE_WORD_LIMIT
        && features.history_user_msgs > N_CONT;
    if long_or_code || continuation {
        Some(apply_cost_quality_bias(Tier::Medium, cost_quality))
    } else {
        None
    }
}

/// Caps the final tier at `Medium` for a remote-origin request, regardless of which rung of
/// the ladder produced it — mirrors the existing `deny_remote_deep` downgrade (mobile/
/// server.rs:412-423) so tier and depth ceilings stay in lockstep for remote requests.
fn apply_remote_ceiling(tier: Tier, remote_origin: bool) -> Tier {
    if remote_origin && tier > Tier::Medium {
        Tier::Medium
    } else {
        tier
    }
}

/// Pick a tier for one turn/pipeline-stage. Ladder (first match wins, remote ceiling applies
/// last regardless of source):
/// 1. [`tier_intent::detect`] on `msg` — an explicit phrase ALWAYS wins.
/// 2. `ctx.depth` — `Deep`→`Thinking`, `Quick`→`Fast`.
/// 3. Heuristic — long message / code fence / continuation guard → `Medium`, biased by
///    `cost_quality`.
/// 4. Otherwise `None` (session default stands).
///
/// `msg` MUST be the genuine user message only (never tool output/assistant text — see the
/// module-level injection invariant); `cost_quality` is the 0–10 slider (phase 7).
pub fn select_tier(msg: &str, ctx: RouteCtx, cost_quality: u8) -> TierDecision {
    let features = RouteFeatures::extract(msg, &ctx);

    let (tier, source) = if let Some(t) = tier_intent::detect(msg) {
        (Some(t), DecisionSource::ExplicitPhrase)
    } else if let Some(t) = depth_tier(ctx.depth) {
        (Some(t), DecisionSource::Depth)
    } else if let Some(t) = heuristic_tier(&features, cost_quality) {
        (Some(t), DecisionSource::Heuristic)
    } else {
        (None, DecisionSource::Default)
    };

    let tier = tier.map(|t| apply_remote_ceiling(t, ctx.remote_origin));

    TierDecision { tier, source, features }
}

/// Mirrors the `deny_remote_deep` remote-origin check (`haily-io::mobile::server`, which sets
/// `adapter_id: "mobile"` — the ONLY remote-transport literal any adapter uses; GUI/CLI/
/// Telegram all route through an in-process or already-authenticated local channel). Feeds
/// `RouteCtx.remote_origin` so the tier ceiling stays in lockstep with the existing depth
/// ceiling for a remote request.
pub fn is_remote_adapter(adapter_id: &str) -> bool {
    adapter_id == "mobile"
}

/// Parses the optional `llm.escalation.egress` preference override (`localonly` |
/// `allowcloud`, case-insensitive). `None` for an absent/unrecognized value, meaning the
/// primary-backend-locality derivation in `agent::turn` applies unmodified.
pub fn parse_egress_override(value: &str) -> Option<Egress> {
    match value.to_lowercase().as_str() {
        "localonly" => Some(Egress::LocalOnly),
        "allowcloud" => Some(Egress::AllowCloud),
        _ => None,
    }
}

/// Streams `req` at `*current_tier`, escalating exactly once to a higher tier on a
/// PRE-FIRST-TOKEN failure — the chat "dead-end rescue" (phase 4). Replaces the turn loop's
/// plain `llm.complete_stream(req)` call.
///
/// Contract (LOCKED, red-team):
/// 1. Cancellation is checked BEFORE any escalation logic — the user's own stop request must
///    never trigger a costlier retry; it propagates the original error immediately.
/// 2. Otherwise consults `policy.next_tier`, capped by `egress`/`llm.highest_local_tier()`.
///    `policy.enabled == false` (e.g. a routing-disabled turn's policy) makes this always
///    `None` — identical to a single plain `complete_stream` attempt, no rescue.
/// 3. No-op guard: a `next` tier that resolves to the SAME backend model as the current
///    attempt (a local-only config collapses `Thinking`/`Ultra` to one loaded GGUF; an
///    unconfigured tier override falls back to the session default either way) is skipped —
///    retrying an identical model wastes a request for nothing.
/// 4. One step max: the escalated retry's own failure propagates as-is, never a second rescue.
///
/// On a successful escalated retry, `*current_tier` is updated so every subsequent tool-loop
/// iteration in THIS turn reuses the rescued tier instead of re-escalating from scratch; a
/// fresh turn always starts over from its own `select_tier` decision (the tier is never
/// persisted past the turn it was computed in).
pub async fn open_stream_with_escalation(
    llm: &LlmRouter,
    current_tier: &mut Option<Tier>,
    req: CompletionRequest,
    policy: &EscalationPolicy,
    egress: Egress,
    cancel: &CancellationToken,
) -> anyhow::Result<mpsc::Receiver<StreamChunk>> {
    match llm.complete_stream_tiered(*current_tier, req.clone()).await {
        Ok(rx) => Ok(rx),
        Err(e) => {
            if cancel.is_cancelled() {
                return Err(e);
            }
            let snapshot = llm.snapshot();
            let highest_local = llm.highest_local_tier();
            // `None` means "session default" — anchor the ladder at the default model's own
            // resolved tier so the escalation math has a real ordinal starting point; an
            // unrecognized default model fails safe to `Fast` (the floor), still buying
            // exactly one rescue step rather than silently skipping escalation altogether.
            let effective_current =
                current_tier.unwrap_or_else(|| snapshot.session_tier(&[]).unwrap_or(Tier::Fast));
            let Some(next) = policy.next_tier(effective_current, 1, egress, highest_local) else {
                return Err(e);
            };
            if snapshot.model_for_tier(Some(next)) == snapshot.model_for_tier(*current_tier) {
                return Err(e);
            }
            match llm.complete_stream_tiered(Some(next), req).await {
                Ok(rx) => {
                    *current_tier = Some(next);
                    Ok(rx)
                }
                Err(e2) => Err(e2),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(depth: DepthMode, history_user_msgs: usize, remote_origin: bool) -> RouteCtx {
        RouteCtx { depth, history_user_msgs, remote_origin }
    }

    // -- explicit phrase: always wins, anchored --------------------------------------

    #[test]
    fn explicit_upward_phrase_wins_over_default_and_is_anchored() {
        let decision = select_tier("nghĩ kỹ về kiến trúc này", ctx(DepthMode::Normal, 0, false), 7);
        assert_eq!(decision.tier, Some(Tier::Thinking));
        assert_eq!(decision.source, DecisionSource::ExplicitPhrase);
    }

    #[test]
    fn explicit_phrase_mid_body_does_not_fire() {
        let msg = "hãy nói về câu nghĩ kỹ trong tiếng Việt và giải thích ý nghĩa của nó trong \
                   văn hóa giao tiếp hàng ngày của người Việt Nam";
        let decision = select_tier(msg, ctx(DepthMode::Normal, 0, false), 7);
        assert_eq!(decision.source, DecisionSource::Default);
        assert_eq!(decision.tier, None);
    }

    #[test]
    fn explicit_phrase_beats_depth_mode_when_both_present() {
        // DepthMode says Quick (→Fast) but the message explicitly asks to think hard.
        let decision = select_tier("think hard", ctx(DepthMode::Quick, 0, false), 7);
        assert_eq!(decision.tier, Some(Tier::Thinking));
        assert_eq!(decision.source, DecisionSource::ExplicitPhrase);
    }

    // -- DepthMode mapping ------------------------------------------------------------

    #[test]
    fn deep_maps_to_thinking_and_quick_maps_to_fast() {
        let deep = select_tier("plain message", ctx(DepthMode::Deep, 0, false), 7);
        assert_eq!(deep.tier, Some(Tier::Thinking));
        assert_eq!(deep.source, DecisionSource::Depth);

        let quick = select_tier("plain message", ctx(DepthMode::Quick, 0, false), 7);
        assert_eq!(quick.tier, Some(Tier::Fast));
        assert_eq!(quick.source, DecisionSource::Depth);
    }

    // -- continuation guard -----------------------------------------------------------

    #[test]
    fn continuation_guard_floors_short_followup_at_medium_in_substantive_history() {
        // 3 words, well past N_CONT prior user messages.
        let decision = select_tier("ok fix that", ctx(DepthMode::Normal, N_CONT + 1, false), 7);
        assert_eq!(decision.tier, Some(Tier::Medium));
        assert_eq!(decision.source, DecisionSource::Heuristic);
    }

    #[test]
    fn short_followup_with_little_history_falls_through_to_default() {
        let decision = select_tier("ok fix that", ctx(DepthMode::Normal, 1, false), 7);
        assert_eq!(decision.tier, None);
        assert_eq!(decision.source, DecisionSource::Default);
    }

    #[test]
    fn long_message_or_code_fence_triggers_heuristic_medium() {
        let long_msg = "word ".repeat(W_HIGH + 1);
        let decision = select_tier(&long_msg, ctx(DepthMode::Normal, 0, false), 7);
        assert_eq!(decision.tier, Some(Tier::Medium));
        assert_eq!(decision.source, DecisionSource::Heuristic);

        let code_msg = "please review\n```rust\nfn x() {}\n```";
        let decision = select_tier(code_msg, ctx(DepthMode::Normal, 0, false), 7);
        assert_eq!(decision.tier, Some(Tier::Medium));
        assert_eq!(decision.source, DecisionSource::Heuristic);
    }

    // -- cost_quality bias at boundary values ------------------------------------------

    #[test]
    fn cost_quality_boundary_values_bias_the_heuristic_tier() {
        let long_msg = "word ".repeat(W_HIGH + 1);
        let at = |cq: u8| select_tier(&long_msg, ctx(DepthMode::Normal, 0, false), cq).tier;

        assert_eq!(at(0), Some(Tier::Fast), "0 biases one step down from Medium");
        assert_eq!(at(3), Some(Tier::Fast), "3 is still in the down-bias band");
        assert_eq!(at(7), Some(Tier::Medium), "7 is neutral");
        assert_eq!(at(8), Some(Tier::Thinking), "8 biases one step up from Medium");
        assert_eq!(at(10), Some(Tier::Thinking), "10 is still in the up-bias band, capped");
    }

    #[test]
    fn ultra_is_never_reachable_via_heuristic_or_cost_quality_knob() {
        let long_msg = "word ".repeat(W_HIGH + 1);
        for cq in 0..=10u8 {
            let decision = select_tier(&long_msg, ctx(DepthMode::Normal, 0, false), cq);
            assert_ne!(decision.tier, Some(Tier::Ultra), "cost_quality={cq} must never reach Ultra");
        }
    }

    // -- remote-origin ceiling ----------------------------------------------------------

    #[test]
    fn remote_origin_caps_final_tier_at_medium_even_for_explicit_phrase() {
        let decision = select_tier("nghĩ kỹ về kiến trúc này", ctx(DepthMode::Normal, 0, true), 7);
        assert_eq!(decision.tier, Some(Tier::Medium));
        assert_eq!(decision.source, DecisionSource::ExplicitPhrase, "source is unaffected by the cap");
    }

    #[test]
    fn remote_origin_caps_deep_depth_mode_at_medium() {
        let decision = select_tier("plain message", ctx(DepthMode::Deep, 0, true), 7);
        assert_eq!(decision.tier, Some(Tier::Medium));
    }

    #[test]
    fn remote_origin_does_not_affect_a_tier_already_at_or_below_medium() {
        let decision = select_tier("trả lời nhanh", ctx(DepthMode::Normal, 0, true), 7);
        assert_eq!(decision.tier, Some(Tier::Fast));
    }

    // -- features round-trip into routing_decisions columns 1:1 ------------------------

    #[test]
    fn features_round_trip_into_routing_decision_columns() {
        let decision = select_tier("nghĩ kỹ", ctx(DepthMode::Deep, 3, false), 7);
        let features = &decision.features;

        // Exactly the fields `NewRoutingDecision` (haily-db, migration 0031) expects —
        // usize -> i64, has_code -> bool, depth -> its wire label, source -> its wire label.
        let new_row = haily_db::queries::routing_decisions::NewRoutingDecision {
            turn_id: "t",
            run_id: None,
            context_kind: "chat",
            stage_kind: None,
            chosen_tier: decision.tier.map(tier_label),
            escalated_to: None,
            decision_source: decision.source.as_label(),
            cost_quality: 7,
            feature_msg_words: features.msg_words as i64,
            feature_has_code: features.has_code,
            feature_history_user_msgs: features.history_user_msgs as i64,
            feature_depth: features.depth_label,
            escalation_trigger: None,
            prior_failures: 0,
        };

        assert_eq!(new_row.feature_msg_words, 2);
        assert!(!new_row.feature_has_code);
        assert_eq!(new_row.feature_history_user_msgs, 3);
        assert_eq!(new_row.feature_depth, "deep");
        assert_eq!(new_row.decision_source, "explicit_phrase");
        assert_eq!(new_row.chosen_tier, Some("thinking"));
    }

    // -- injection invariant -------------------------------------------------------------

    /// `RouteCtx` has no field for raw history text — only `history_user_msgs`, a trusted
    /// count. This proves the API shape itself enforces the invariant: two calls with the
    /// same message and the same count always agree, and only bumping the COUNT (never any
    /// simulated "bloated tool-result text", which there is structurally nowhere to pass)
    /// changes the decision.
    #[test]
    fn injection_bloated_text_cannot_reach_routing_only_counts_can() {
        let small = ctx(DepthMode::Normal, 1, false);
        let a = select_tier("ok fix that", small, 5);
        let b = select_tier("ok fix that", small, 5);
        assert_eq!(a.tier, b.tier);
        assert_eq!(a.source, b.source);

        // A poisoned/huge "assembled history" string, if it existed, has no field to occupy
        // on `RouteCtx` — the only way to move the decision is the trusted count below.
        let bumped = ctx(DepthMode::Normal, N_CONT + 5, false);
        let c = select_tier("ok fix that", bumped, 5);
        assert_ne!(c.tier, a.tier, "only the trusted history_user_msgs count can change the decision");
        assert_eq!(c.source, DecisionSource::Heuristic);
    }

    // -- routing_enabled=false plumbing --------------------------------------------------

    #[test]
    fn tier_decision_default_is_session_default_with_no_tier() {
        let decision = TierDecision::default();
        assert_eq!(decision.tier, None);
        assert_eq!(decision.source, DecisionSource::Default);
    }

    // -- remote-adapter / egress-override helpers ----------------------------------------

    #[test]
    fn is_remote_adapter_true_only_for_mobile() {
        assert!(is_remote_adapter("mobile"));
        assert!(!is_remote_adapter("gui"));
        assert!(!is_remote_adapter("telegram"));
        assert!(!is_remote_adapter("cli"));
    }

    #[test]
    fn parse_egress_override_parses_known_values_case_insensitively() {
        assert_eq!(parse_egress_override("localonly"), Some(Egress::LocalOnly));
        assert_eq!(parse_egress_override("LocalOnly"), Some(Egress::LocalOnly));
        assert_eq!(parse_egress_override("allowcloud"), Some(Egress::AllowCloud));
        assert_eq!(parse_egress_override("ALLOWCLOUD"), Some(Egress::AllowCloud));
        assert_eq!(parse_egress_override("garbage"), None);
        assert_eq!(parse_egress_override(""), None);
    }
}

#[cfg(test)]
mod escalation_tests {
    //! `open_stream_with_escalation` integration tests — a REAL `LlmRouter` (cloud backend)
    //! against a scripted loopback SSE/TCP mock server, mirroring the exact mock-server
    //! technique `agent::turn`'s own test modules use (per this file's per-module-helper
    //! convention, duplicated rather than shared — see `sub_turn.rs`'s doc for that
    //! precedent). No real network call: bound to `127.0.0.1:0`, OS-assigned port.
    use super::*;
    use haily_llm::{LlmConfig, Message};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn cloud_config(base_url: String) -> LlmConfig {
        LlmConfig {
            cloud_api_keys: vec!["test-key".to_string()],
            cloud_base_url: base_url,
            cloud_model: "test-model".to_string(),
            ..LlmConfig::default()
        }
    }

    fn req() -> CompletionRequest {
        CompletionRequest::simple(vec![Message::user("hi")])
    }

    fn enabled_policy() -> EscalationPolicy {
        EscalationPolicy { failures_before_escalation: 1, max_tier: Tier::Thinking, enabled: true }
    }

    /// Every connection this server accepts either fails immediately (closes without a
    /// valid response — a pre-first-token error) or succeeds with `content`, per
    /// `fail_first_n` calls. Returns `(base_url, call_count)` so a test can assert exactly
    /// how many connections were actually attempted.
    fn spawn_server(fail_first_n: usize, content: &'static str) -> (String, Arc<AtomicUsize>) {
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_srv = Arc::clone(&call_count);
        // A synchronous std listener bound before the async server task starts, so the
        // base_url is available to the caller immediately (no race on first connect).
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let addr = std_listener.local_addr().expect("local_addr");
        let listener = TcpListener::from_std(std_listener).expect("tokio listener");

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let call_count = Arc::clone(&call_count_srv);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = stream.read(&mut buf).await;
                    let n = call_count.fetch_add(1, Ordering::SeqCst);
                    if n < fail_first_n {
                        // A malformed status line forces an IMMEDIATE synchronous parse
                        // error in the HTTP client (never a timeout-dependent wait for a
                        // connection-reset to propagate) — a deterministic, fast way to
                        // simulate a pre-first-token stream-init failure.
                        let _ = stream.write_all(b"not a valid http response\r\n\r\n").await;
                        let _ = stream.shutdown().await;
                        return;
                    }
                    let delta = serde_json::json!({
                        "choices": [{ "delta": { "content": content } }]
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

        (format!("http://{addr}"), call_count)
    }

    #[tokio::test]
    async fn cancellation_never_escalates_and_propagates_the_original_error() {
        let (base_url, call_count) = spawn_server(usize::MAX, "unused");
        let llm = LlmRouter::init(cloud_config(base_url)).await;
        let cancel = CancellationToken::new();
        cancel.cancel(); // already cancelled BEFORE the call

        let mut current_tier = None;
        let result = open_stream_with_escalation(
            &llm,
            &mut current_tier,
            req(),
            &enabled_policy(),
            Egress::AllowCloud,
            &cancel,
        )
        .await;

        assert!(result.is_err(), "a pre-first-token failure on a cancelled turn must propagate");
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "cancellation must prevent any escalated retry — exactly one attempt"
        );
        assert_eq!(current_tier, None, "tier must be left untouched on a cancelled failure");
    }

    #[tokio::test]
    async fn disabled_policy_never_escalates() {
        let (base_url, call_count) = spawn_server(usize::MAX, "unused");
        let llm = LlmRouter::init(cloud_config(base_url)).await;
        let cancel = CancellationToken::new();
        let disabled = EscalationPolicy { failures_before_escalation: 1, max_tier: Tier::Thinking, enabled: false };

        let mut current_tier = None;
        let result = open_stream_with_escalation(
            &llm, &mut current_tier, req(), &disabled, Egress::AllowCloud, &cancel,
        )
        .await;

        assert!(result.is_err(), "a disabled policy must behave exactly like plain complete_stream — no rescue");
        assert_eq!(call_count.load(Ordering::SeqCst), 1, "disabled policy must never attempt a retry");
    }

    #[tokio::test]
    async fn no_op_guard_skips_retry_when_next_tier_resolves_to_the_same_model() {
        // No `tier_models` override configured — `model_for_tier(Some(next))` and
        // `model_for_tier(None)` both fall back to the SAME session default model, so the
        // no-op guard must fire before a second connection is ever attempted.
        let (base_url, call_count) = spawn_server(usize::MAX, "unused");
        let llm = LlmRouter::init(cloud_config(base_url)).await;
        let cancel = CancellationToken::new();

        let mut current_tier = None;
        let result = open_stream_with_escalation(
            &llm, &mut current_tier, req(), &enabled_policy(), Egress::AllowCloud, &cancel,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "an identical resolved model must never be retried"
        );
        assert_eq!(current_tier, None);
    }

    #[tokio::test]
    async fn pre_first_token_error_escalates_once_to_a_distinct_tier_model() {
        // Fails the FIRST connection, succeeds every connection after — proving the
        // escalated retry actually reaches the server and succeeds.
        let (base_url, call_count) = spawn_server(1, "Escalated answer.");
        let mut config = cloud_config(base_url);
        // A distinct Medium override so `model_for_tier(Some(Medium)) != model_for_tier(None)`
        // — the no-op guard must NOT fire here.
        config.tier_models.medium = Some("distinct-medium-model".to_string());
        let llm = LlmRouter::init(config).await;
        let cancel = CancellationToken::new();

        let mut current_tier = None;
        let result = open_stream_with_escalation(
            &llm, &mut current_tier, req(), &enabled_policy(), Egress::AllowCloud, &cancel,
        )
        .await;

        assert!(result.is_ok(), "the escalated retry must succeed");
        assert_eq!(call_count.load(Ordering::SeqCst), 2, "exactly one retry attempt");
        assert_eq!(
            current_tier,
            Some(Tier::Medium),
            "current_tier must be updated to the tier that actually succeeded"
        );
    }

    #[tokio::test]
    async fn a_successful_first_attempt_never_touches_the_escalation_path() {
        let (base_url, call_count) = spawn_server(0, "Plain answer.");
        let llm = LlmRouter::init(cloud_config(base_url)).await;
        let cancel = CancellationToken::new();

        let mut current_tier = None;
        let result = open_stream_with_escalation(
            &llm, &mut current_tier, req(), &enabled_policy(), Egress::AllowCloud, &cancel,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(call_count.load(Ordering::SeqCst), 1, "a clean success must never retry");
        assert_eq!(current_tier, None, "tier must be untouched when no escalation ever happened");
    }
}
