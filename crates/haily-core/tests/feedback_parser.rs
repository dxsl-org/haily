/// Tests for the feedback signal detector — haily-core side.
use haily_core::feedback_parser::detect_feedback;
use haily_kms::feedback::FeedbackSignal;

#[test]
fn detects_positive_thumbs_up() {
    let sig = detect_feedback("👍 hay lắm");
    assert!(matches!(sig, Some(FeedbackSignal::Positive)));
}

#[test]
fn detects_positive_word() {
    let sig = detect_feedback("tốt, cảm ơn nhé");
    assert!(matches!(sig, Some(FeedbackSignal::Positive)));
}

#[test]
fn detects_negative_thumbs_down() {
    let sig = detect_feedback("👎 sai rồi");
    assert!(matches!(sig, Some(FeedbackSignal::Negative { .. })));
}

#[test]
fn detects_negative_too_long() {
    let sig = detect_feedback("ngắn thôi, dài quá");
    assert!(
        matches!(sig, Some(FeedbackSignal::Negative { topic: Some(ref t) }) if t == "response_length")
    );
}

#[test]
fn detects_correction_vietnamese() {
    // try_parse_correction works on the lowercased string to avoid byte-offset/char-boundary
    // issues — so old/new are lowercase. Preference keys are case-insensitive so this is correct.
    let sig = detect_feedback("không phải Hà Nội mà là Hồ Chí Minh");
    assert!(matches!(
        sig,
        Some(FeedbackSignal::Correction { ref old, ref new })
            if old.contains("hà nội") && new.contains("hồ chí minh")
    ));
}

#[test]
fn no_signal_on_normal_message() {
    let sig = detect_feedback("hôm nay thời tiết thế nào?");
    assert!(sig.is_none());
}

#[test]
fn no_signal_on_empty() {
    let sig = detect_feedback("");
    assert!(sig.is_none());
}
