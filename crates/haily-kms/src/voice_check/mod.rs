//! Deterministic, offline voice-consistency checkers (Phase 10 of assistant-depth).
//!
//! **NO LLM-as-judge — locked project invariant.** Every check here is a substring match,
//! character-class scan, or numeric threshold; none may call a model. An LLM-as-judge would
//! itself be subject to the exact model-drift risk this eval exists to catch — a swap
//! silently changing the JUDGE's opinion is indistinguishable from a real regression. See
//! `.agents/260707-assistant-depth/reports/voice-eval-criteria.md` sections A/B for the
//! criteria implemented below and `tests/voice_consistency.rs` for the fixtures (that test
//! file wins if it and this doc ever disagree).
//!
//! `check_voice` is pure — same inputs always produce the same report, no I/O, no shared
//! state — so it can be re-run offline against freshly captured samples after a model swap
//! (criteria doc section C) without touching CI or a live model.
//!
//! Split across `mod.rs` (Section A — universal invariants) and `soul_rules.rs` (Section B —
//! per-soul required/forbidden/density checks) to respect the <200-line file-size convention;
//! `check_voice` below is the single public seam that composes both.

mod soul_rules;

use crate::Soul;

/// Language the fixture declares as correct (not inferred from the response) — catches the
/// model answering in the wrong language relative to the expected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedLanguage {
    Vn,
    En,
}

/// One rule violation: `rule` is a stable tag tests match on, `reason` is CI-readable detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceFailure {
    pub rule: &'static str,
    pub reason: String,
}

impl VoiceFailure {
    pub(crate) fn new(rule: &'static str, reason: impl Into<String>) -> Self {
        Self { rule, reason: reason.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VoiceCheckReport {
    pub failures: Vec<VoiceFailure>,
}

impl VoiceCheckReport {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn has_failure(&self, rule: &str) -> bool {
        self.failures.iter().any(|f| f.rule == rule)
    }
}

const SYCOPHANCY_PHRASES: &[&str] = &[
    "câu hỏi hay",
    "tất nhiên!",
    "tất nhiên rồi",
    "tôi rất vui được giúp",
    "tuyệt vời!",
    "rất vui được hỗ trợ",
];
const AI_DISCLAIMER_PHRASES: &[&str] =
    &["với vai trò là trợ lý ai", "as an ai assistant", "as an ai language model"];
const VN_DIACRITICS: &str = "àáạảãâầấậẩẫăằắặẳẵèéẹẻẽêềếệểễìíịỉĩòóọỏõôồốộổỗơờớợởỡùúụủũưừứựửữỳýỵỷỹđ";
const VN_STOPWORDS: &[&str] = &["và", "là", "của", "không", "được", "có", "này", "cho", "với"];
const MAX_RESPONSE_CHARS: usize = 700;

pub(crate) fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// Trims leading/trailing punctuation so `"nhé!"` still matches bare-word particle sets.
pub(crate) fn tokenize_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

pub fn count_occurrences_ci(text: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    text.to_lowercase().matches(&needle.to_lowercase()).count()
}

pub fn forbidden_sycophancy_present(text: &str) -> Option<&'static str> {
    SYCOPHANCY_PHRASES.iter().copied().find(|p| contains_ci(text, p))
}

pub fn forbidden_ai_disclaimer_present(text: &str) -> Option<&'static str> {
    AI_DISCLAIMER_PHRASES.iter().copied().find(|p| contains_ci(text, p))
}

pub fn repeated_apology_count(text: &str) -> usize {
    count_occurrences_ci(text, "xin lỗi")
}

pub fn has_vietnamese_diacritics(text: &str) -> bool {
    text.to_lowercase().chars().any(|c| VN_DIACRITICS.contains(c))
}

fn has_vn_stopword(text: &str) -> bool {
    let words = tokenize_words(text);
    VN_STOPWORDS.iter().any(|w| words.iter().any(|tok| tok == w))
}

pub fn language_matches(text: &str, expected: ExpectedLanguage) -> bool {
    let vn_signal = has_vietnamese_diacritics(text) || has_vn_stopword(text);
    match expected {
        ExpectedLanguage::Vn => vn_signal,
        ExpectedLanguage::En => !vn_signal,
    }
}

pub fn within_length_bound(text: &str, max_chars: usize) -> bool {
    text.chars().count() <= max_chars
}

/// Excludes the Arrows block (U+2190-21FF) — "→" is a required Tete marker, not a violation.
fn is_emoji_char(c: char) -> bool {
    matches!(c as u32, 0x1F1E6..=0x1F1FF | 0x1F300..=0x1FAFF | 0x2600..=0x27BF)
}

pub fn contains_emoji(text: &str) -> bool {
    text.chars().any(is_emoji_char)
}

/// The seam: candidate assistant message + active soul + expected language in, deterministic
/// pass/fail-with-reasons out. The same function this module's tests exercise is what a
/// future drift check re-runs against freshly captured post-model-swap samples.
pub fn check_voice(text: &str, soul: &Soul, expected_language: ExpectedLanguage) -> VoiceCheckReport {
    let mut failures = Vec::new();

    if let Some(hit) = forbidden_sycophancy_present(text) {
        failures.push(VoiceFailure::new("forbidden_sycophancy", format!("matched \"{hit}\"")));
    }
    if let Some(hit) = forbidden_ai_disclaimer_present(text) {
        failures.push(VoiceFailure::new("forbidden_ai_disclaimer", format!("matched \"{hit}\"")));
    }
    let apology_count = repeated_apology_count(text);
    if apology_count > 1 {
        failures.push(VoiceFailure::new("repeated_apology", format!("\"xin lỗi\" appears {apology_count} times")));
    }
    if !language_matches(text, expected_language) {
        failures.push(VoiceFailure::new("language_mismatch", format!("expected {expected_language:?}")));
    }
    if !within_length_bound(text, MAX_RESPONSE_CHARS) {
        failures.push(VoiceFailure::new("length_bound", format!("exceeds {MAX_RESPONSE_CHARS} chars")));
    }

    match soul {
        Soul::Haily => soul_rules::check_haily(text, &mut failures),
        Soul::Tete => soul_rules::check_tete(text, &mut failures),
        Soul::Hoami => soul_rules::check_hoami(text, &mut failures),
        Soul::Lungmat => soul_rules::check_lungmat(text, &mut failures),
    }

    VoiceCheckReport { failures }
}
