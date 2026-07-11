/// Detects feedback signals in user messages. Signal handling lives in haily-kms::feedback.
///
/// Precision rules (phase-08, F16): the pre-phase-08 version matched on bare substring
/// containment anywhere in the message, which fired on ordinary conversation containing
/// an incidental word ("hay là mình đi ăn" ≈ "let's go eat instead" tripped the positive
/// "hay" pattern; any long message that happened to contain "not" tripped a correction
/// parse). A signal now fires only when:
///   (a) the message is short (`SHORT_MESSAGE_WORD_LIMIT` words or fewer), OR the
///       pattern sits at the very start of the (trimmed, lowercased) message, AND
///   (b) the pattern itself is not one of the highest-noise single words — bare "hay"
///       and bare "đúng" are dropped entirely; the English correction trigger requires
///       a leading "no, it's" rather than a bare "not " substring.
pub use haily_kms::feedback::{apply_feedback_signal, FeedbackSignal};

/// Messages at or under this word count are short enough that ANY matched pattern is
/// assumed to BE the message's whole intent (e.g. "tốt lắm", "sai rồi") rather than an
/// incidental word inside a longer, unrelated sentence.
///
/// `pub(crate)` so the phase-7 depth phrase-mapper (`crate::depth`) reuses the EXACT
/// same precision rule instead of copying it — a depth phrase incidentally present in a
/// long pasted/tool-shaped body must not fire, for the same reason a feedback phrase must
/// not (DEP-minor: a copied guard risks security-contract drift between the two).
pub(crate) const SHORT_MESSAGE_WORD_LIMIT: usize = 6;

const POS_PATTERNS: &[&str] = &[
    "👍",
    "tốt",
    "đúng rồi",
    "chính xác",
    "perfect",
    "great",
    "cảm ơn",
    "thank",
    "good",
];
const NEG_PATTERNS: &[&str] = &[
    "👎",
    "sai",
    "không phải",
    "đừng làm vậy",
    "không đúng",
    "dài quá",
    "ngắn thôi",
    "quá dài",
    "wrong",
    "bad",
    "stop",
];

/// Word count of a trimmed message — used for the short-message anchor exemption.
/// Whitespace-split is sufficient here (not full Unicode word segmentation): both
/// Vietnamese and English feedback phrases in practice are space-separated tokens,
/// and the exact count only needs to distinguish "short" from "not short," not be
/// linguistically precise.
///
/// `pub(crate)` — shared with `crate::depth` (see [`SHORT_MESSAGE_WORD_LIMIT`]).
pub(crate) fn word_count(msg: &str) -> usize {
    msg.split_whitespace().count()
}

/// A pattern signal is precise enough to fire when the message is short OR the
/// pattern sits at the very start — both computed against the same lowercased,
/// trimmed string the caller already produced, so byte offsets stay consistent.
///
/// `pub(crate)` — this IS the reusable source-guard primitive the phase-7 depth
/// phrase-mapper consumes (do NOT copy it into `depth.rs`): "fire only when the whole
/// short message IS the signal, or the signal leads the message" is exactly what stops a
/// phrase buried in a longer pasted/tool body from being read as intent.
pub(crate) fn is_anchored(lower_trimmed: &str, pattern: &str, short: bool) -> bool {
    short || lower_trimmed.starts_with(pattern)
}

fn try_parse_correction(msg: &str) -> Option<FeedbackSignal> {
    // Work entirely on the lowercased string to avoid byte-offset/char-boundary
    // mismatches when to_lowercase() changes byte lengths (e.g. Turkish İ).
    let lower = msg.to_lowercase();
    let trimmed = lower.trim();
    let short = word_count(trimmed) <= SHORT_MESSAGE_WORD_LIMIT;

    // "không phải X mà là Y" — the full Vietnamese correction shape, not a bare
    // substring check. Requires both the "không phải" anchor AND one of the
    // "mà (là)" separators to actually find an old/new pair.
    if let Some(pos) = lower.find("không phải") {
        if is_anchored(trimmed, "không phải", short) {
            let after = &lower[pos + "không phải".len()..];
            for sep in &[" mà là ", " mà "] {
                if let Some(sep_pos) = after.find(sep) {
                    let old = after[..sep_pos].trim().to_string();
                    let new = after[sep_pos + sep.len()..].trim().to_string();
                    if !old.is_empty() && !new.is_empty() {
                        return Some(FeedbackSignal::Correction { old, new });
                    }
                }
            }
        }
    }

    // English correction trigger: dropped the bare "not " substring match (fired on
    // any long message merely containing the word "not") in favor of requiring a
    // leading "no, it's ... but ..." shape.
    if trimmed.starts_with("no, it's") || trimmed.starts_with("no it's") {
        let after = trimmed
            .trim_start_matches("no, it's")
            .trim_start_matches("no it's");
        if let Some(sep_pos) = after.find(" but ") {
            let old = after[..sep_pos].trim().to_string();
            let new = after[sep_pos + " but ".len()..].trim().to_string();
            if !old.is_empty() && !new.is_empty() {
                return Some(FeedbackSignal::Correction { old, new });
            }
        }
    }

    None
}

fn infer_negative_topic(msg: &str) -> Option<String> {
    let lower = msg.to_lowercase();
    if lower.contains("dài") || lower.contains("ngắn thôi") || lower.contains("long") {
        return Some("response_length".to_string());
    }
    if lower.contains("ngôn ngữ") || lower.contains("language") {
        return Some("language".to_string());
    }
    if lower.contains("phong cách") || lower.contains("tone") || lower.contains("style") {
        return Some("tone".to_string());
    }
    None
}

/// Scan a user message for a feedback signal. Returns `None` if no signal found.
pub fn detect_feedback(msg: &str) -> Option<FeedbackSignal> {
    if let Some(corr) = try_parse_correction(msg) {
        return Some(corr);
    }

    let lower = msg.to_lowercase();
    let trimmed = lower.trim();
    let short = word_count(trimmed) <= SHORT_MESSAGE_WORD_LIMIT;

    for pat in NEG_PATTERNS {
        if lower.contains(*pat) && is_anchored(trimmed, pat, short) {
            return Some(FeedbackSignal::Negative {
                topic: infer_negative_topic(msg),
            });
        }
    }
    for pat in POS_PATTERNS {
        if lower.contains(*pat) && is_anchored(trimmed, pat, short) {
            return Some(FeedbackSignal::Positive);
        }
    }
    None
}

#[cfg(test)]
mod precision_tests {
    //! F16 true-positive / false-positive table (phase-08 spec's Test Scenario
    //! Matrix — "Feedback table: 6 true-positive + 6 false-positive cases all pass").
    use super::*;

    // -- True positives: short, unambiguous feedback --------------------------

    #[test]
    fn tp_tot_lam_is_positive() {
        assert!(matches!(
            detect_feedback("tốt lắm"),
            Some(FeedbackSignal::Positive)
        ));
    }

    #[test]
    fn tp_thumbs_up_is_positive() {
        assert!(matches!(
            detect_feedback("👍"),
            Some(FeedbackSignal::Positive)
        ));
    }

    #[test]
    fn tp_sai_roi_is_negative() {
        assert!(matches!(
            detect_feedback("sai rồi"),
            Some(FeedbackSignal::Negative { .. })
        ));
    }

    #[test]
    fn tp_dai_qua_is_negative_response_length() {
        let sig = detect_feedback("dài quá").expect("must fire");
        match sig {
            FeedbackSignal::Negative { topic } => {
                assert_eq!(topic.as_deref(), Some("response_length"));
            }
            other => panic!("expected Negative, got {other:?}"),
        }
    }

    #[test]
    fn tp_khong_phai_a_ma_la_b_is_correction() {
        let sig = detect_feedback("không phải A mà là B").expect("must fire");
        match sig {
            FeedbackSignal::Correction { old, new } => {
                assert_eq!(old, "a");
                assert_eq!(new, "b");
            }
            other => panic!("expected Correction, got {other:?}"),
        }
    }

    #[test]
    fn tp_no_its_x_but_y_is_correction() {
        let sig = detect_feedback("No, it's Hanoi but Saigon").expect("must fire");
        match sig {
            FeedbackSignal::Correction { old, new } => {
                assert_eq!(old, "hanoi");
                assert_eq!(new, "saigon");
            }
            other => panic!("expected Correction, got {other:?}"),
        }
    }

    // -- False positives: must NOT fire ---------------------------------------

    #[test]
    fn fp_hay_la_di_an_does_not_fire() {
        // "hay" is dropped entirely as a bare pattern; this is also short (5 words)
        // but must not match anything since "hay" is no longer in POS_PATTERNS.
        assert!(detect_feedback("hay là mình đi ăn tối nay").is_none());
    }

    #[test]
    fn fp_cai_nay_hay_day_nhung_does_not_fire() {
        assert!(detect_feedback(
            "cái này hay đấy nhưng mình nghĩ nên thử cách khác xem sao đã nhé"
        )
        .is_none());
    }

    #[test]
    fn fp_long_message_containing_not_does_not_fire() {
        let msg = "I was thinking about the plan and I am not sure if this is going to \
                    work out the way we expected it to, but let's see how it goes";
        assert!(detect_feedback(msg).is_none());
    }

    #[test]
    fn fp_bare_dung_mid_sentence_does_not_fire() {
        // Bare "đúng" is dropped as a positive trigger — a long sentence merely
        // containing it must not fire.
        let msg = "tôi nghĩ là đúng nhưng để chắc chắn thì mình nên kiểm tra lại toàn bộ dữ liệu trước khi quyết định";
        assert!(detect_feedback(msg).is_none());
    }

    #[test]
    fn fp_long_message_mentioning_style_without_complaint_does_not_fire() {
        let msg =
            "phong cách viết code ở đây khá là thú vị và mình học được nhiều thứ mới từ dự án này";
        assert!(detect_feedback(msg).is_none());
    }

    #[test]
    fn fp_stop_word_mid_long_sentence_does_not_fire() {
        let msg = "we can stop by the store on the way home if you want to pick up some \
                    snacks before the movie starts tonight";
        assert!(detect_feedback(msg).is_none());
    }

    // -- Anchor-at-start still fires even in a longer message ------------------

    #[test]
    fn pattern_at_message_start_fires_even_when_not_short() {
        let msg = "sai rồi, mình đã nói là chủ nhật tuần sau chứ không phải tuần này đâu bạn ơi";
        assert!(matches!(
            detect_feedback(msg),
            Some(FeedbackSignal::Negative { .. })
        ));
    }
}
