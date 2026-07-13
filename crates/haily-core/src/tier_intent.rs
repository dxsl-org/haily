//! Explicit tier-intent phrase detection (Auto Model Routing R1, phase 3).
//!
//! Near-verbatim clone of [`crate::depth`]'s phrase-mapper shape: [`detect`] reuses
//! `feedback_parser`'s anchoring precision rule (`is_anchored`/`word_count`) so a phrase
//! incidentally present in a long pasted/tool-shaped body cannot steer routing — the same
//! source-guard contract `depth.rs` documents at its top. The call site (`routing::select_tier`,
//! wired in Phase 4) must pass ONLY the genuine user message, never assembled context or tool
//! output.
//!
//! Overlap with `depth.rs`: `UPWARD_PHRASES`/`DOWNWARD_PHRASES` share several entries with
//! `DEEP_PHRASES`/`QUICK_PHRASES` by design — a phrase that sets `DepthMode::Deep` must always
//! map to `Tier::Thinking` here, and `DepthMode::Quick` to `Tier::Fast`, so the two independent
//! signals (judgment depth vs. model tier) never disagree about what an explicit user request
//! meant.

use crate::feedback_parser::{is_anchored, word_count, SHORT_MESSAGE_WORD_LIMIT};
use haily_llm::Tier;

/// VN/EN phrases that request a stronger tier (`Tier::Thinking`). Shares its VN/EN core with
/// `depth::DEEP_PHRASES` ("nghĩ kỹ", "phân tích sâu") plus tier-specific additions ("cẩn thận
/// nhé", "think carefully", "deep dive", "analyze thoroughly") that ask for rigor without
/// necessarily requesting the full Deep judgment pipeline.
const UPWARD_PHRASES: &[&str] = &[
    "nghĩ kỹ",
    "suy nghĩ kỹ",
    "phân tích sâu",
    "cẩn thận nhé",
    "think hard",
    "think carefully",
    "deep dive",
    "analyze thoroughly",
];

/// VN/EN phrases that request a cheaper/faster tier (`Tier::Fast`). Haily-original list
/// (researcher-01: no published downward-intent inventory exists) — kept small by design,
/// grown from `routing_decisions` log data rather than speculative expansion.
const DOWNWARD_PHRASES: &[&str] = &[
    "trả lời nhanh",
    "nhanh thôi",
    "ngắn gọn thôi",
    "quick answer",
    "just quickly",
    "briefly",
];

/// Detect an explicit tier request in a GENUINE user message. Returns `None` when no phrase
/// fires (the caller falls through to the next rung of the `select_tier` ladder).
///
/// A phrase fires only when it is anchored (the whole short message IS the request, or the
/// phrase leads the message) — the reused `feedback_parser` precision rule — so a tier word
/// buried in a longer pasted/tool-shaped body never changes routing. Upward is checked before
/// downward: if a message somehow contains both, the stronger explicit request wins.
pub fn detect(msg: &str) -> Option<Tier> {
    let lower = msg.to_lowercase();
    let trimmed = lower.trim();
    let short = word_count(trimmed) <= SHORT_MESSAGE_WORD_LIMIT;

    for pat in UPWARD_PHRASES {
        if lower.contains(*pat) && is_anchored(trimmed, pat, short) {
            return Some(Tier::Thinking);
        }
    }
    for pat in DOWNWARD_PHRASES {
        if lower.contains(*pat) && is_anchored(trimmed, pat, short) {
            return Some(Tier::Fast);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_upward_phrase_sets_thinking() {
        assert_eq!(detect("nghĩ kỹ về kiến trúc này"), Some(Tier::Thinking));
        assert_eq!(detect("think hard"), Some(Tier::Thinking));
    }

    #[test]
    fn short_downward_phrase_sets_fast() {
        assert_eq!(detect("trả lời nhanh"), Some(Tier::Fast));
        assert_eq!(detect("just quickly"), Some(Tier::Fast));
    }

    #[test]
    fn a_plain_message_sets_no_tier() {
        assert_eq!(detect("giúp tôi viết một email cho sếp"), None);
        assert_eq!(detect("what's the weather today"), None);
    }

    /// SOURCE CONTRACT (LOCKED, mirrors depth.rs): a tier phrase mid-body in a long,
    /// non-anchored message must NOT fire — anchoring is what stops a copy-pasted/tool-shaped
    /// body from steering routing (prompt-injection guard).
    #[test]
    fn phrase_mid_body_in_a_long_unanchored_message_does_not_fire() {
        let msg = "hãy nói về câu nghĩ kỹ trong tiếng Việt và giải thích ý nghĩa của nó trong \
                   văn hóa giao tiếp hàng ngày của người Việt Nam";
        assert_eq!(
            detect(msg),
            None,
            "a phrase mid-body in a long text must not fire (source-guard reuse)"
        );
    }

    #[test]
    fn anchored_upward_phrase_leads_a_longer_message_still_fires() {
        let msg = "nghĩ kỹ giúp tôi, tôi cần một bản kế hoạch chi tiết cho toàn bộ dự án này";
        assert_eq!(detect(msg), Some(Tier::Thinking));
    }
}
