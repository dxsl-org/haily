/// Deterministic Vietnamese + English eval set for the feedback signal detector
/// (Phase 11, B8). Distinct from `feedback_parser.rs`'s smoke tests: this file is a
/// labeled fixture *table* that reports a pass rate, so a future regression in
/// `feedback_parser.rs`'s anchor/short-message precision rules (see that file's F16
/// doc comment) is caught as a number, not just a handful of pass/fail asserts.
///
/// NO LLM-as-judge (locked invariant, Decision 25): every case below is a plain
/// `==` comparison against a hand-labeled expected classification.
///
/// This phase MEASURES; it does not fix (phase-11 Risk Notes). If a case's expected
/// label documents a known false-negative (recall gap) of the anchor design, that is
/// intentional and called out inline — not a bug for this phase to patch.
use haily_core::feedback_parser::detect_feedback;
use haily_kms::feedback::FeedbackSignal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Positive,
    Negative,
    Correction,
    None,
}

fn classify(sig: &Option<FeedbackSignal>) -> Kind {
    match sig {
        Some(FeedbackSignal::Positive) => Kind::Positive,
        Some(FeedbackSignal::Negative { .. }) => Kind::Negative,
        Some(FeedbackSignal::Correction { .. }) => Kind::Correction,
        None => Kind::None,
    }
}

/// (message, expected classification, note). The note documents WHY the label is
/// what it is when it isn't obvious from the message alone (anchor rule, bare-word
/// exclusion, or a known recall gap).
const CASES: &[(&str, Kind, &str)] = &[
    // -- Positive: short, unambiguous --------------------------------------------
    ("tốt lắm", Kind::Positive, "short VN positive"),
    ("👍", Kind::Positive, "emoji-only positive"),
    ("cảm ơn bạn nhiều", Kind::Positive, "short VN thanks"),
    ("Perfect!", Kind::Positive, "short EN positive"),
    ("chính xác đó", Kind::Positive, "short VN exact-match phrase"),
    ("đúng rồi bạn ơi", Kind::Positive, "short VN, multi-word pattern anchors it"),
    ("Great job today!", Kind::Positive, "short EN positive"),
    (
        "Cảm ơn vì đã giúp mình hôm nay nhé bạn hiền ơi",
        Kind::Positive,
        "long but pattern sits at the very start (start-anchor rule)",
    ),
    // -- Negative: short, unambiguous --------------------------------------------
    ("sai rồi", Kind::Negative, "short VN negative"),
    ("👎", Kind::Negative, "emoji-only negative"),
    ("không đúng đâu", Kind::Negative, "short VN, not a correction (no 'mà' separator)"),
    ("Đừng làm vậy nữa nhé", Kind::Negative, "short VN imperative-negative"),
    ("Dài quá đi", Kind::Negative, "short VN, infers response_length topic"),
    ("Ngắn thôi nha", Kind::Negative, "short VN, infers response_length topic"),
    ("This is wrong", Kind::Negative, "short EN negative"),
    ("Please stop doing that", Kind::Negative, "short EN negative"),
    ("Bad answer, try again", Kind::Negative, "short EN negative"),
    (
        "Sai.",
        Kind::Negative,
        "bare 'sai' still fires (unlike bare 'hay'/'đúng', which are excluded — F16)",
    ),
    (
        "không phải vậy đâu",
        Kind::Negative,
        "correction shape fails (no 'mà' separator) → falls through to the 'không phải' \
         NEG_PATTERNS catch-all, short so anchored",
    ),
    (
        "KHÔNG ĐÚNG!",
        Kind::Negative,
        "uppercase VN diacritics must case-fold correctly through to_lowercase()",
    ),
    // -- Correction: full "old → new" shape recognized ---------------------------
    (
        "không phải màu đỏ mà là màu xanh",
        Kind::Correction,
        "VN correction, ' mà là ' separator",
    ),
    (
        "không phải 3 giờ chiều mà 5 giờ chiều",
        Kind::Correction,
        "VN correction, bare ' mà ' separator (no 'là')",
    ),
    (
        "Không Phải Hà Nội Mà Là Đà Nẵng",
        Kind::Correction,
        "mixed-case VN correction, case-folds via to_lowercase()",
    ),
    ("No, it's Tuesday but Wednesday", Kind::Correction, "EN correction, comma form"),
    ("no it's Monday but Friday", Kind::Correction, "EN correction, no-comma form"),
    // -- None: must NOT fire (precision over recall by design) -------------------
    ("", Kind::None, "empty message"),
    ("hôm nay thời tiết thế nào?", Kind::None, "neutral question, no pattern present"),
    (
        "Bạn có thể giúp mình đặt lịch họp lúc 3 giờ chiều mai được không?",
        Kind::None,
        "bare 'không' is never a pattern on its own",
    ),
    (
        "Hay là mình đi xem phim tối nay nhé",
        Kind::None,
        "bare 'hay' is excluded entirely (F16 false-positive fix)",
    ),
    (
        "Hôm nay thời tiết khá tốt nhưng chắc chiều lại mưa to đấy bạn ơi",
        Kind::None,
        "known recall gap: 'tốt' present but neither short nor start-anchored — \
         intentional precision/recall tradeoff, not a bug for this phase to fix",
    ),
    (
        "Mình cũng cảm ơn vì bạn đã giúp đỡ mình quá nhiều trong dự án lần này thật đó",
        Kind::None,
        "known recall gap: 'cảm ơn' present but mid-sentence, long, not anchored",
    ),
    (
        "We could stop for coffee on the way if you feel like it this afternoon",
        Kind::None,
        "'stop' present but mid-sentence, long, not anchored",
    ),
    (
        "Có thể là do mình làm sai bước nào đó nhưng chưa chắc, để mình kiểm tra lại \
         toàn bộ quy trình đã",
        Kind::None,
        "'sai' present but mid-sentence, long, not anchored",
    ),
];

/// Baseline recorded 2026-07-06 against the anchor-based parser (F16 precision
/// rules) on the fixture set above: 33/33 pass. This assertion guards against
/// REGRESSION below that measured baseline — it is not a target to chase upward by
/// tuning the parser here (out of scope for this phase; see phase-11 Risk Notes).
const BASELINE_PASS_RATE: f64 = 1.0;

#[test]
fn vn_en_feedback_eval_matches_expected_labels() {
    let total = CASES.len();
    let mut passed = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (msg, expected, note) in CASES {
        let actual = classify(&detect_feedback(msg));
        if actual == *expected {
            passed += 1;
        } else {
            failures.push(format!(
                "{msg:?} ({note}): expected {expected:?}, got {actual:?}"
            ));
        }
    }

    let pass_rate = passed as f64 / total as f64;
    println!(
        "VN+EN feedback-parser eval: {passed}/{total} ({:.1}%) pass rate",
        pass_rate * 100.0
    );
    for f in &failures {
        println!("  FAIL: {f}");
    }

    assert!(
        pass_rate >= BASELINE_PASS_RATE,
        "feedback-parser eval regressed below baseline {:.1}% ({passed}/{total} passed):\n{}",
        BASELINE_PASS_RATE * 100.0,
        failures.join("\n")
    );
}
