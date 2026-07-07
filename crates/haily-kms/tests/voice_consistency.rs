//! Voice-consistency eval (Phase 10 of assistant-depth) — the model-upgrade drift gate.
//!
//! Source of truth for the criteria below:
//! `.agents/260707-assistant-depth/reports/voice-eval-criteria.md`. Per that doc, if this
//! file and the criteria doc ever disagree, THIS FILE WINS — it is what CI actually runs.
//!
//! **NO LLM-as-judge.** Every assertion here calls `haily_kms::voice_check`, which is pure
//! string/char/count logic — see that module's doc comment for the rationale.
//!
//! ## Model-upgrade drift protocol (criteria doc section C)
//! 1. This test runs in CI on every build — no separate trigger needed.
//! 2. After a model swap (llama.cpp checkpoint or `haily-llm/src/router.rs` cloud model
//!    change): capture real sample outputs offline per soul against representative prompts,
//!    then add them as new fixtures below (or run them ad hoc through
//!    `haily_kms::voice_check::check_voice`) — reuse these same checker functions, do not
//!    build a second eval mechanism.
//! 3. A newly-failing fixture after a swap is a real regression signal, not flakiness — fix
//!    `system_prompt::soul_style_block` / the invariant blocks until it passes again. Do not
//!    loosen a checker to make a regression disappear.

use haily_kms::voice_check::{self, check_voice, ExpectedLanguage};
use haily_kms::Soul;

/// Asserts a fixture passes every deterministic check.
fn assert_passes(name: &str, text: &str, soul: &Soul, lang: ExpectedLanguage) {
    let report = check_voice(text, soul, lang);
    assert!(report.passed(), "fixture '{name}' expected to pass but failed: {:?}", report.failures);
}

/// Asserts a fixture fails EXACTLY the named rule and no other — this is what proves the
/// checkers are precise (a bad sample authored to probe one rule must not incidentally trip
/// a different one).
fn assert_fails_only(name: &str, text: &str, soul: &Soul, lang: ExpectedLanguage, rule: &str) {
    let report = check_voice(text, soul, lang);
    assert_eq!(
        report.failures.len(),
        1,
        "fixture '{name}' expected exactly 1 failure ({rule}) but got: {:?}",
        report.failures
    );
    assert!(report.has_failure(rule), "fixture '{name}' expected failure '{rule}', got: {:?}", report.failures);
}

// ---------------------------------------------------------------------------------------
// Section A — universal invariants (probed via the Haily soul, which adds the fewest
// soul-specific constraints on top).
// ---------------------------------------------------------------------------------------

const GOOD_VN: &str = "Đã kiểm tra dữ liệu. Server đang chạy ổn định, không có lỗi nào trong 24 giờ qua. \
Bạn cần thêm thông tin gì không.";
const GOOD_EN: &str = "Server is stable. No errors in the last 24 hours. Anything else you need?";

#[test]
fn section_a_good_samples_pass() {
    assert_passes("good_vn", GOOD_VN, &Soul::Haily, ExpectedLanguage::Vn);
    assert_passes("good_en", GOOD_EN, &Soul::Haily, ExpectedLanguage::En);
}

#[test]
fn section_a_forbidden_sycophancy_fails_only_that_check() {
    assert_fails_only(
        "bad_sycophancy",
        "Câu hỏi hay! Để tôi kiểm tra ngay.",
        &Soul::Haily,
        ExpectedLanguage::Vn,
        "forbidden_sycophancy",
    );
}

#[test]
fn section_a_forbidden_ai_disclaimer_fails_only_that_check() {
    assert_fails_only(
        "bad_disclaimer",
        "Với vai trò là trợ lý AI, tôi sẽ giúp bạn ngay.",
        &Soul::Haily,
        ExpectedLanguage::Vn,
        "forbidden_ai_disclaimer",
    );
}

#[test]
fn section_a_repeated_apology_fails_only_that_check() {
    assert_fails_only(
        "bad_apology",
        "Xin lỗi, để tôi sửa lại. Xin lỗi vì đã chậm trễ.",
        &Soul::Haily,
        ExpectedLanguage::Vn,
        "repeated_apology",
    );
}

#[test]
fn section_a_language_mismatch_fails_only_that_check() {
    // Same text as GOOD_VN — the only thing that changes is the fixture's declared
    // expected language, isolating the language check from every other rule.
    assert_fails_only("bad_language", GOOD_VN, &Soul::Haily, ExpectedLanguage::En, "language_mismatch");
}

#[test]
fn section_a_length_bound_fails_only_that_check() {
    let long_text = "Đây là một câu trả lời rất dài để kiểm tra giới hạn độ dài của phản hồi trong hệ thống. "
        .repeat(9);
    assert!(long_text.chars().count() > 700, "fixture must actually exceed the 700-char bound");
    assert_fails_only("bad_length", &long_text, &Soul::Haily, ExpectedLanguage::Vn, "length_bound");
}

// ---------------------------------------------------------------------------------------
// Section B — per-soul required/forbidden/density checks.
// ---------------------------------------------------------------------------------------

#[test]
fn haily_bad_samples_fail_only_their_intended_check() {
    assert_fails_only(
        "haily_bad_emoji",
        "Đã xong việc rồi 😊",
        &Soul::Haily,
        ExpectedLanguage::Vn,
        "haily_forbidden_emoji",
    );
    assert_fails_only(
        "haily_bad_particle",
        "Xong việc rồi nhé.",
        &Soul::Haily,
        ExpectedLanguage::Vn,
        "haily_forbidden_particle",
    );
    assert_fails_only(
        "haily_bad_tone",
        "Xong rồi! Ổn rồi! Ngon rồi!",
        &Soul::Haily,
        ExpectedLanguage::Vn,
        "haily_tone_exclamation",
    );
}

const GOOD_TETE: &str = "Trạng thái: ổn định. CPU: 12%. RAM: 40%. Không lỗi.";

#[test]
fn tete_good_sample_passes() {
    assert_passes("good_tete", GOOD_TETE, &Soul::Tete, ExpectedLanguage::Vn);
}

#[test]
fn tete_bad_samples_fail_only_their_intended_check() {
    assert_fails_only(
        "tete_bad_required",
        "Server đang chạy bình thường không có gì đặc biệt trong hôm nay cả",
        &Soul::Tete,
        ExpectedLanguage::Vn,
        "tete_required_marker",
    );
    assert_fails_only("tete_bad_emoji", "Data: ổn 😀", &Soul::Tete, ExpectedLanguage::Vn, "tete_forbidden_emoji");
    assert_fails_only(
        "tete_bad_particle",
        "Data: xong nhé",
        &Soul::Tete,
        ExpectedLanguage::Vn,
        "tete_forbidden_particle",
    );
    assert_fails_only(
        "tete_bad_opener",
        "Chào bạn, data: ổn",
        &Soul::Tete,
        ExpectedLanguage::Vn,
        "tete_forbidden_opener",
    );

    let long_tete = format!("Data: {}", "trạng thái ổn định ".repeat(10));
    assert!(long_tete.split_whitespace().count() > 40, "fixture must actually exceed the 40-word bound");
    assert_fails_only("tete_bad_length", &long_tete, &Soul::Tete, ExpectedLanguage::Vn, "tete_length_bound");
}

const GOOD_HOAMI: &str = "Xong rồi nhé. Anh cần gì thêm không.";

#[test]
fn hoami_good_sample_passes() {
    assert_passes("good_hoami", GOOD_HOAMI, &Soul::Hoami, ExpectedLanguage::Vn);
}

#[test]
fn hoami_bad_samples_fail_only_their_intended_check() {
    assert_fails_only(
        "hoami_bad_required",
        "Xong rồi. Anh cần gì thêm không.",
        &Soul::Hoami,
        ExpectedLanguage::Vn,
        "hoami_required_particle",
    );
    // A run-on single sentence deliberately stacking 3 particle hits — synthetic (not
    // representative prose) purely to trip the density-ceiling math, not the required check.
    assert_fails_only(
        "hoami_bad_density",
        "Nhé, xong rồi nhé, cảm ơn nhé.",
        &Soul::Hoami,
        ExpectedLanguage::Vn,
        "hoami_particle_density",
    );
}

const GOOD_LUNGMAT: &str = "Xong rồi! Ngon lành 🎉";

#[test]
fn lungmat_good_sample_passes() {
    assert_passes("good_lungmat", GOOD_LUNGMAT, &Soul::Lungmat, ExpectedLanguage::Vn);
}

#[test]
fn lungmat_bad_sample_fails_only_its_intended_check() {
    assert_fails_only(
        "lungmat_bad_required",
        "Xong rồi. Ổn cả.",
        &Soul::Lungmat,
        ExpectedLanguage::Vn,
        "lungmat_required_energy",
    );
}

#[test]
fn lungmat_deescalated_reply_is_a_documented_checker_limitation_not_a_bug() {
    // Per voice-eval-criteria.md section B (Lungmat): the doc's own de-escalation rule
    // ("khi tình huống nghiêm trọng → tự động giảm tông") needs situational judgment a
    // single response cannot supply, so this checker intentionally does NOT try to infer
    // "serious situation" from the text. A correctly de-escalated reply to a serious
    // situation legitimately carries fewer/no energy markers and will therefore trip the
    // generic `lungmat_required_energy` rule — that is an accepted, documented tradeoff,
    // not a defect to special-case away here (doing so would require the exact
    // text-based seriousness inference the criteria doc forbids).
    let serious_but_correct_reply = "Deadline gấp rồi, mình tập trung sửa lỗi này trước đã, không đùa lúc này.";

    assert!(!voice_check::contains_emoji(serious_but_correct_reply));
    assert!(!serious_but_correct_reply.contains('!'));
    assert!(!serious_but_correct_reply.contains("..."));

    let report = check_voice(serious_but_correct_reply, &Soul::Lungmat, ExpectedLanguage::Vn);
    assert!(
        report.has_failure("lungmat_required_energy"),
        "expected the documented false-positive on a correctly de-escalated reply"
    );
}
