/// Detects feedback signals in user messages. Signal handling lives in haily-kms::feedback.
pub use haily_kms::feedback::{apply_feedback_signal, FeedbackSignal};

const POS_PATTERNS: &[&str] = &[
    "👍", "tốt", "hay", "đúng rồi", "chính xác", "đúng", "perfect", "great",
    "cảm ơn", "thank", "good",
];
const NEG_PATTERNS: &[&str] = &[
    "👎", "sai", "không phải", "đừng làm vậy", "đừng", "không đúng",
    "dài quá", "ngắn thôi", "quá dài", "wrong", "bad", "stop",
];

fn try_parse_correction(msg: &str) -> Option<FeedbackSignal> {
    let lower = msg.to_lowercase();
    for marker in &["không phải", "not "] {
        if let Some(pos) = lower.find(marker) {
            let after = &msg[pos + marker.len()..];
            for sep in &[" mà là ", " mà ", " it's ", " but "] {
                if let Some(sep_pos) = after.to_lowercase().find(sep) {
                    let old = after[..sep_pos].trim().to_string();
                    let new = after[sep_pos + sep.len()..].trim().to_string();
                    if !old.is_empty() && !new.is_empty() {
                        return Some(FeedbackSignal::Correction { old, new });
                    }
                }
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
    for pat in NEG_PATTERNS {
        if lower.contains(*pat) {
            return Some(FeedbackSignal::Negative { topic: infer_negative_topic(msg) });
        }
    }
    for pat in POS_PATTERNS {
        if lower.contains(*pat) {
            return Some(FeedbackSignal::Positive);
        }
    }
    None
}
