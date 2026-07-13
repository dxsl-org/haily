//! Tier decision core (Auto Model Routing R1, phase 3): [`select_tier`] picks the
//! [`haily_llm::Tier`] a turn/pipeline-stage should run on, purely from trusted-origin inputs.
//!
//! Called by nobody yet — Phase 4 wires this into `agent::turn` and threads
//! [`TierDecision::features`] into `routing_decisions` (phase 2's log table). Kept standalone
//! and fully unit-testable so the decision ladder is provable in isolation before it touches
//! the hot chat path.
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
use haily_llm::Tier;

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
}
