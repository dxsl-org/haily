//! Chat-intent classifier for pipeline auto-detection (Pipeline Activation & Wiring, phase 2).
//!
//! [`classify`] answers one question: does this no-slash chat message read as an explicit
//! request to plan or build code? It is the ONLY signal `haily-app::trigger` uses to offer a
//! confirm-gated pipeline launch — never a silent auto-launch (Security Considerations, phase
//! file). Precision borrows the EXACT anchor rule `feedback_parser` already proved out (F16):
//! a phrase fires only when the message is short (`SHORT_MESSAGE_WORD_LIMIT` words or fewer) OR
//! the phrase leads the trimmed, lowercased message. That rule is what stops a coding phrase
//! buried in a long pasted diff, tool-call output, or unrelated conversation from reading as
//! intent — reused via `crate::feedback_parser::{is_anchored, word_count,
//! SHORT_MESSAGE_WORD_LIMIT}` (both `pub(crate)` in that module) rather than copied, so the two
//! precision contracts can never drift apart (the same reason `crate::depth` reuses them).
//!
//! The lexicon is deliberately narrow multi-word phrases ("build this feature", "fix this bug")
//! rather than bare verbs ("build", "implement", "fix") — a bare verb is common enough in
//! ordinary conversation (and in a short leading sentence about something else entirely) that it
//! would false-positive far too often even under the anchor rule.
use crate::feedback_parser::{is_anchored, word_count, SHORT_MESSAGE_WORD_LIMIT};
use crate::RunKind;
use haily_types::RequestOrigin;

const PLAN_PATTERNS: &[&str] = &[
    "plan this feature",
    "plan the implementation of",
    "write a plan for this feature",
    "design the architecture for",
    "lên kế hoạch cho tính năng",
    "thiết kế kiến trúc cho",
];

const BUILD_PATTERNS: &[&str] = &[
    "build this feature",
    "implement this feature",
    "code this feature",
    "fix this bug",
    "write the code for this",
    "refactor this code",
    "viết code cho tính năng",
    "sửa lỗi này trong code",
    "code tính năng này",
    "thêm tính năng này vào code",
];

/// Classify a no-slash chat message as a coding-pipeline intent, or `None` if it does not read
/// as one. `origin` is checked here too (not just by the caller) as a defense-in-depth mirror of
/// `trigger::resolve`'s own `RequestOrigin::Chat` gate — a `Cli`-origin message (the eval bypass
/// path) must never intent-launch even if this function is ever called directly.
pub fn classify(msg: &str, origin: RequestOrigin) -> Option<RunKind> {
    if origin != RequestOrigin::Chat {
        return None;
    }

    let lower = msg.to_lowercase();
    let trimmed = lower.trim();
    let short = word_count(trimmed) <= SHORT_MESSAGE_WORD_LIMIT;

    for pat in PLAN_PATTERNS {
        if lower.contains(*pat) && is_anchored(trimmed, pat, short) {
            return Some(RunKind::Plan);
        }
    }
    for pat in BUILD_PATTERNS {
        if lower.contains(*pat) && is_anchored(trimmed, pat, short) {
            return Some(RunKind::Build);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- True positives ---------------------------------------------------------

    #[test]
    fn tp_short_english_build_request() {
        assert_eq!(
            classify("implement this feature", RequestOrigin::Chat),
            Some(RunKind::Build)
        );
    }

    #[test]
    fn tp_leading_english_build_request_in_a_longer_sentence() {
        assert_eq!(
            classify(
                "build this feature please, it's urgent",
                RequestOrigin::Chat
            ),
            Some(RunKind::Build)
        );
    }

    #[test]
    fn tp_short_english_fix_request() {
        assert_eq!(
            classify("fix this bug now", RequestOrigin::Chat),
            Some(RunKind::Build)
        );
    }

    #[test]
    fn tp_short_english_plan_request() {
        assert_eq!(
            classify("plan this feature first", RequestOrigin::Chat),
            Some(RunKind::Plan)
        );
    }

    #[test]
    fn tp_short_vietnamese_build_request() {
        assert_eq!(
            classify("viết code cho tính năng đăng nhập", RequestOrigin::Chat),
            Some(RunKind::Build)
        );
    }

    #[test]
    fn tp_short_vietnamese_plan_request() {
        assert_eq!(
            classify("lên kế hoạch cho tính năng thanh toán", RequestOrigin::Chat),
            Some(RunKind::Plan)
        );
    }

    // -- False positives: must NOT fire ------------------------------------------

    #[test]
    fn fp_bare_verb_mid_unrelated_sentence_does_not_fire() {
        let msg = "we should reconsider whether we really need to fix this bug given the low \
                    priority and how little time is left in this sprint to build anything else";
        assert_eq!(classify(msg, RequestOrigin::Chat), None);
    }

    #[test]
    fn fp_incidental_coding_words_in_long_unrelated_message_does_not_fire() {
        let msg = "I was reading an article about how machine learning models are trained and it \
                    mentioned some code snippets briefly but nothing about fixing anything really";
        assert_eq!(classify(msg, RequestOrigin::Chat), None);
    }

    #[test]
    fn fp_ambiguous_short_message_mentioning_build_does_not_fire() {
        // Contains "build" but not the anchored phrase "build this feature".
        assert_eq!(
            classify("let's build a sandcastle together", RequestOrigin::Chat),
            None
        );
    }

    #[test]
    fn fp_long_pasted_diff_with_phrase_not_leading_does_not_fire() {
        let msg = "diff --git a/src/main.rs b/src/main.rs\n// TODO: fix this bug later, someone \
                    left a comment about it but nobody has picked it up yet in the backlog";
        assert_eq!(classify(msg, RequestOrigin::Chat), None);
    }

    #[test]
    fn fp_long_message_containing_exact_phrase_not_leading_does_not_fire() {
        let msg = "there was some discussion in the meeting about whether we should implement \
                    this feature this quarter or push it to next quarter given our current load";
        assert_eq!(classify(msg, RequestOrigin::Chat), None);
    }

    // -- Cli-origin exclusion (SEC-H) --------------------------------------------

    #[test]
    fn cli_origin_never_intent_launches_even_with_clearly_coding_shaped_text() {
        assert_eq!(
            classify("implement this feature", RequestOrigin::Cli),
            None,
            "Cli origin (the eval bypass path) must never intent-launch"
        );
    }
}
