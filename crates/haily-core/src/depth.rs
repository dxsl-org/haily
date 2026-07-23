//! Judgment-depth tiers (Sub-Agent + Skill Architecture phase 7).
//!
//! [`DepthMode`] (`Quick`/`Normal`/`Deep`, default `Normal`) is set per request via a GUI
//! toggle OR a VN/EN phrase in the GENUINE user message — never from tool output, pasted
//! content, or any model-influenced text. `Deep` buys multi-stream judgment (judge panel,
//! refuter votes, apex judge — see [`crate::pipeline::judge`]) at explicit 3–5× cost.
//!
//! Two LOCKED invariants live here:
//! - **Depth is only detected from a real user message.** [`detect_depth`] reuses
//!   `feedback_parser`'s anchoring precision rule (`is_anchored`/`word_count`) rather than
//!   copying it, so a phrase incidentally present in a long pasted/tool body cannot set
//!   depth (DEP-minor: a copied guard risks contract drift). The call site (`run_turn`)
//!   passes ONLY `req.message`.
//! - **The harness NEVER auto-escalates to Deep.** [`parity_hint`] is TEXT-ONLY: when the
//!   session model tier is below `Thinking`, it returns one advisory line suggesting Deep +
//!   its cost. It never blocks, never changes egress, and never flips the mode.

use crate::feedback_parser::{is_anchored, word_count, SHORT_MESSAGE_WORD_LIMIT};
use haily_llm::Tier;

/// `DepthMode` is defined in the leaf `haily-types` crate (so `Request` can carry it
/// typed) and re-exported here as the crate-local home of everything depth-related.
pub use haily_types::DepthMode;

/// VN/EN phrases that request the Deep tier. Matched against the lowercased, trimmed user
/// message under the SAME anchoring rule feedback detection uses.
const DEEP_PHRASES: &[&str] = &[
    "làm kỹ",
    "phân tích sâu",
    "nghĩ kỹ",
    "kỹ càng",
    "deep",
    "think hard",
    "think deeply",
    "be thorough",
];

/// VN/EN phrases that request the Quick tier.
const QUICK_PHRASES: &[&str] = &[
    "làm nhanh",
    "nhanh thôi",
    "trả lời nhanh",
    "quick",
    "be quick",
    "fast",
];

/// Detect a depth request in a GENUINE user message. Returns `None` when no phrase fires
/// (the caller keeps the request's existing [`DepthMode`], typically the GUI toggle value).
///
/// A phrase fires only when it is anchored (the whole short message IS the request, or the
/// phrase leads the message) — the reused `feedback_parser` precision rule — so a depth
/// word buried in a longer pasted/tool-shaped body never changes depth. Deep is checked
/// before Quick: if a message somehow contains both, the stronger explicit request wins,
/// and Deep still requires its own exact anchored match (never inferred).
pub fn detect_depth(msg: &str) -> Option<DepthMode> {
    let lower = msg.to_lowercase();
    let trimmed = lower.trim();
    let short = word_count(trimmed) <= SHORT_MESSAGE_WORD_LIMIT;

    for pat in DEEP_PHRASES {
        if lower.contains(*pat) && is_anchored(trimmed, pat, short) {
            return Some(DepthMode::Deep);
        }
    }
    for pat in QUICK_PHRASES {
        if lower.contains(*pat) && is_anchored(trimmed, pat, short) {
            return Some(DepthMode::Quick);
        }
    }
    None
}

/// The effective depth for a turn: a phrase in the genuine user message OVERRIDES the
/// request's toggle-set default; absent a phrase, the toggle value stands. Never returns
/// Deep unless it was explicitly toggled or explicitly phrased — the harness contribution
/// is zero (no inference).
pub fn effective_depth(request_depth: DepthMode, user_message: &str) -> DepthMode {
    detect_depth(user_message).unwrap_or(request_depth)
}

/// The TEXT-ONLY parity hint (phase 7 LOCKED): when the session model tier is below
/// `Thinking`, weak-model output benefits most from Deep's multi-stream judgment, so we
/// surface ONE advisory line the user can act on — we NEVER auto-escalate. `None` (no
/// hint) when the tier is `Thinking`/`Ultra` (Deep buys less there) — an UNKNOWN tier
/// (`None`) is treated as below-Thinking (fail toward informing the user Deep exists).
pub fn parity_hint(session_tier: Option<Tier>) -> Option<String> {
    let below_thinking = match session_tier {
        Some(t) => t < Tier::Thinking,
        None => true,
    };
    if below_thinking {
        Some(
            "Gợi ý: mô hình hiện tại thiên về tốc độ. Với việc cần phán đoán kỹ, bật chế độ \
             Sâu (Deep) để có phán đoán đa luồng — lưu ý chi phí ước tính cao gấp 3–5 lần."
                .to_string(),
        )
    } else {
        None
    }
}

/// A per-depth playbook addendum for the NON-coding delegation domains (researcher /
/// creator-writer) — prompt-level only (no new engine; the coding pipeline is where depth
/// gets real stage-graph support). Names the kit-pack lens/playbook the sub-agent should
/// follow; `None` for Normal or for a domain with no depth variant. Kept as static text so
/// it composes into the sub-turn task without a DB round-trip.
pub fn research_depth_addendum(domain: &str, depth: DepthMode) -> Option<&'static str> {
    match (domain, depth) {
        ("researcher", DepthMode::Deep) => Some(
            "\n\n## Depth: Deep\nFan out across multiple independent angles before converging: \
             gather from distinct sources/framings, note where they disagree, then synthesize \
             a single grounded answer that cites which angle each claim rests on. Do not average \
             conflicting sources — reconcile them explicitly or flag the unresolved conflict.",
        ),
        ("creator", DepthMode::Deep) => Some(
            "\n\n## Depth: Deep\nDraft more than one framing/outline, weigh them against the \
             stated goal and audience, then commit to one and note why the alternatives were \
             rejected. Depth here is judgment about structure, not more words.",
        ),
        ("researcher" | "creator", DepthMode::Quick) => Some(
            "\n\n## Depth: Quick\nAnswer directly from the most reliable single source/framing. \
             Skip multi-angle fan-out; be concise.",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- detect_depth: explicit-only + source-guard -----------------------------------

    #[test]
    fn short_deep_phrase_sets_deep() {
        assert_eq!(detect_depth("làm kỹ vào"), Some(DepthMode::Deep));
        assert_eq!(detect_depth("think hard"), Some(DepthMode::Deep));
        assert_eq!(
            detect_depth("phân tích sâu giúp tôi"),
            Some(DepthMode::Deep)
        );
    }

    #[test]
    fn short_quick_phrase_sets_quick() {
        assert_eq!(detect_depth("làm nhanh"), Some(DepthMode::Quick));
        assert_eq!(detect_depth("quick answer please"), Some(DepthMode::Quick));
    }

    #[test]
    fn a_plain_message_sets_no_depth() {
        assert_eq!(detect_depth("giúp tôi viết một email cho sếp"), None);
        assert_eq!(detect_depth("what's the weather today"), None);
    }

    /// SOURCE CONTRACT (LOCKED): a depth phrase incidentally buried in a long, non-anchored
    /// body (the shape pasted/tool content takes) must NOT set depth — the reused
    /// `feedback_parser` anchoring rule is what enforces this, so a copy-drift here would be
    /// a security regression.
    #[test]
    fn depth_phrase_inside_a_long_unanchored_body_does_not_fire() {
        let pasted = "Here is a document the user pasted that happens to mention going deep \
                      into the subject and doing think hard style analysis somewhere in the \
                      middle of a very long paragraph that is clearly not a depth request";
        assert_eq!(
            detect_depth(pasted),
            None,
            "a phrase mid-body in a long text must not set depth (source-guard reuse)"
        );
    }

    #[test]
    fn anchored_deep_phrase_leads_a_longer_message_still_fires() {
        // Anchored at the very start, so it IS the request even in a longer message —
        // exactly the feedback_parser anchor-at-start exemption, reused.
        let msg = "làm kỹ nhé, tôi cần bản kế hoạch chi tiết cho toàn bộ dự án này trong tuần";
        assert_eq!(detect_depth(msg), Some(DepthMode::Deep));
    }

    #[test]
    fn effective_depth_phrase_overrides_toggle_but_absence_keeps_it() {
        // A Deep phrase overrides a Normal toggle.
        assert_eq!(
            effective_depth(DepthMode::Normal, "làm kỹ"),
            DepthMode::Deep
        );
        // No phrase → the toggle value stands (here Deep from the GUI).
        assert_eq!(
            effective_depth(DepthMode::Deep, "help me plan a trip"),
            DepthMode::Deep
        );
        // No phrase, Normal toggle → Normal.
        assert_eq!(
            effective_depth(DepthMode::Normal, "help me plan a trip"),
            DepthMode::Normal
        );
    }

    // -- parity_hint: text-only, never escalates --------------------------------------

    #[test]
    fn parity_hint_fires_below_thinking_only() {
        assert!(parity_hint(Some(Tier::Fast)).is_some());
        assert!(parity_hint(Some(Tier::Medium)).is_some());
        assert!(
            parity_hint(None).is_some(),
            "unknown tier fails toward informing"
        );
        assert!(parity_hint(Some(Tier::Thinking)).is_none());
        assert!(parity_hint(Some(Tier::Ultra)).is_none());
    }

    #[test]
    fn research_depth_addendum_is_prompt_level_and_domain_scoped() {
        assert!(research_depth_addendum("researcher", DepthMode::Deep)
            .unwrap()
            .contains("Fan out"));
        assert!(research_depth_addendum("researcher", DepthMode::Normal).is_none());
        // A coding domain has no non-coding addendum (its depth lives in the pipeline).
        assert!(research_depth_addendum("developer", DepthMode::Deep).is_none());
    }
}
