/// Harness Completion phase 5 — label provenance (`derive_label`), Jaccard-based
/// repeat-request/skill-matching, and the anti-reinforcement safety invariant.
use haily_db::queries::skills as db_skills;
use haily_kms::skills::{
    derive_label, find_matching_skill, gate_label_supersedes, is_repeat_request,
    jaccard_similarity, synthesized_playbooks, LabelSource, SkillGates, TaskOutcome,
    EXPLICIT_FEEDBACK_CONFIDENCE, GATE_RESULT_CONFIDENCE, PHRASE_FEEDBACK_CONFIDENCE,
    REPEAT_REQUEST_CONFIDENCE, SYNTH_SKILL_MIN_CONFIDENCE, TOOL_ERROR_RATIO_CONFIDENCE,
    UNDO_LABEL_CONFIDENCE,
};

// ---------------------------------------------------------------------------
// derive_label — priority order + the anti-reinforcement safety invariant
// ---------------------------------------------------------------------------

// m4: the undo signal must stay deliberately near-zero until phase 2's local undos
// have matured it — a `const` assertion (not a runtime `assert!`, which clippy flags
// as vacuous for a compile-time-constant comparison) pins that intent at compile time.
const _: () = assert!(
    UNDO_LABEL_CONFIDENCE < 0.1,
    "UNDO_LABEL_CONFIDENCE must stay deliberately near-zero (m4)"
);

#[test]
fn undo_within_5min_takes_priority_and_uses_the_conservative_confidence() {
    let label = derive_label(TaskOutcome::Success, true, true, false);
    assert_eq!(label.source, LabelSource::UndoWithinN);
    assert_eq!(label.confidence, UNDO_LABEL_CONFIDENCE);
}

#[test]
fn failure_outcome_without_undo_labels_as_tool_error_ratio() {
    let label = derive_label(TaskOutcome::Failure, false, false, false);
    assert_eq!(label.source, LabelSource::ToolErrorRatio);
    assert_eq!(label.confidence, TOOL_ERROR_RATIO_CONFIDENCE);
}

/// M2 review fix: an UNCORROBORATED repeat (no other negative indicator this turn)
/// must NOT read as a failure signal — a user who habitually sends near-duplicate
/// consecutive messages (e.g. a daily "tóm tắt hôm nay" habit) must not have every
/// one of those turns erode an otherwise-healthy skill's confidence.
#[test]
fn uncorroborated_repeat_request_stays_unknown() {
    let label = derive_label(TaskOutcome::Success, false, true, false);
    assert!(
        label.is_unknown(),
        "a benign repeat with no corroborating negative signal must not move confidence, got {:?}",
        label.source
    );
}

/// M2 review fix: a repeat request CORROBORATED by another same-turn negative
/// indicator (here, a `Partial` outcome) DOES label as `RepeatRequest` — the
/// corroboration requirement narrows the signal's precision without disabling it
/// entirely when genuine additional evidence exists.
#[test]
fn corroborated_repeat_request_labels_as_repeat_request() {
    let label = derive_label(TaskOutcome::Success, false, true, true);
    assert_eq!(label.source, LabelSource::RepeatRequest);
    assert_eq!(label.confidence, REPEAT_REQUEST_CONFIDENCE);
}

/// SAFETY (anti-reinforcement invariant, memory 2026-06-21): a turn with no
/// corroborating signal at all must derive `Unknown` — the caller (`haily-core::agent`)
/// is contractually required to SKIP `update_skill_confidence` entirely in this case,
/// never default to a neutral 0.5 reward. This test pins the pure-function half of
/// that contract: `Unknown` is reachable, and `is_unknown()` correctly identifies it.
#[test]
fn success_with_no_undo_and_no_repeat_is_unknown_and_must_not_move_confidence() {
    let label = derive_label(TaskOutcome::Success, false, false, false);
    assert!(
        label.is_unknown(),
        "a plain successful turn with no corroborating signal must be Unknown, not a \
         forced Success label — moving confidence on zero signal is exactly the \
         self-reinforcement loop this design forbids"
    );
    assert_eq!(label.source, LabelSource::Unknown);
}

#[test]
fn partial_outcome_with_no_other_signal_is_also_unknown() {
    // Partial does not, by itself, imply tool_error_ratio (that's Failure-only per
    // derive_label's priority order) — without undo/repeat it must fall through to
    // Unknown rather than being force-labeled.
    let label = derive_label(TaskOutcome::Partial, false, false, false);
    assert!(label.is_unknown());
}

// ---------------------------------------------------------------------------
// m2 — phrase-detected feedback must be capped below an explicit tool signal
// ---------------------------------------------------------------------------

// A compile-time-constant comparison (both are `const`s) — pinned via a `const`
// assertion rather than a runtime `#[test]`, per clippy's assertions-on-constants
// lint. `feedback_downgrade.rs`'s
// `explicit_downgrade_confidence_is_strictly_higher_than_phrase_downgrade` proves the
// SAME property end-to-end through the real downgrade path.
const _: () = assert!(
    PHRASE_FEEDBACK_CONFIDENCE < EXPLICIT_FEEDBACK_CONFIDENCE,
    "m2: a pattern-matched phrase must never carry equal or higher confidence than an explicit feedback_react tool call"
);

// ---------------------------------------------------------------------------
// phase 8 — GateResult label precedence (anti-reinforcement, LOCKED decision #4)
// ---------------------------------------------------------------------------

// A gate result is a hard, reproducible signal — weighted just under an explicit human
// reaction. Pinned at compile time so a future edit cannot silently invert the precedence.
const _: () = assert!(
    GATE_RESULT_CONFIDENCE < EXPLICIT_FEEDBACK_CONFIDENCE || GATE_RESULT_CONFIDENCE <= 0.9,
    "a deterministic gate result must never outrank explicit human feedback"
);
const _: () = assert!(
    GATE_RESULT_CONFIDENCE > TOOL_ERROR_RATIO_CONFIDENCE,
    "a deterministic gate result is strictly stronger evidence than a phrase/error-ratio heuristic"
);

/// The load-bearing precedence rule: a GateResult label may supersede any label EXCEPT an
/// explicit-feedback one, and freely supersedes `None` (no prior label).
#[test]
fn gate_result_never_supersedes_explicit_feedback_but_supersedes_everything_else() {
    assert!(
        !gate_label_supersedes(Some(LabelSource::ExplicitFeedback.as_str())),
        "a gate result MUST NOT overwrite an explicit_feedback label on the same trace"
    );
    assert!(gate_label_supersedes(None), "a gate result freely labels an unlabeled trace");
    assert!(gate_label_supersedes(Some(LabelSource::ToolErrorRatio.as_str())));
    assert!(gate_label_supersedes(Some(LabelSource::PhraseFeedback.as_str())));
    assert!(gate_label_supersedes(Some(LabelSource::UndoWithinN.as_str())));
    assert!(gate_label_supersedes(Some(LabelSource::GateResult.as_str())));
}

// ---------------------------------------------------------------------------
// phase 8 — synthesized-skill injection confidence gate
// ---------------------------------------------------------------------------

#[test]
fn synthesized_skill_below_confidence_floor_never_reaches_the_pool() {
    let mut low = make_skill("s1", "flight-booking", "book a flight ticket for the user");
    low.confidence = SYNTH_SKILL_MIN_CONFIDENCE - 0.01;
    let picked = synthesized_playbooks(
        &[low],
        "please book a flight ticket to hanoi",
        SYNTH_SKILL_MIN_CONFIDENCE,
        3,
        &SkillGates::default(),
    );
    assert!(
        picked.is_empty(),
        "a synthesized skill below the confidence floor must never be injected"
    );
}

#[test]
fn synthesized_skill_at_or_above_floor_and_matching_is_injected_with_visible_provenance() {
    let mut high = make_skill("s1", "flight-booking", "book a flight ticket for the user");
    high.confidence = SYNTH_SKILL_MIN_CONFIDENCE;
    let picked = synthesized_playbooks(
        &[high],
        "please book a flight ticket to hanoi",
        SYNTH_SKILL_MIN_CONFIDENCE,
        3,
        &SkillGates::default(),
    );
    assert_eq!(picked.len(), 1, "a matching skill at the floor must be injected");
    assert!(
        picked[0].0.contains("(synthesized skill)"),
        "provenance must be visible in the heading, got: {}",
        picked[0].0
    );
}

#[test]
fn synthesized_skill_that_does_not_match_the_task_is_not_injected() {
    let mut high = make_skill("s2", "weather-lookup", "check the weather forecast for a city");
    high.confidence = 0.95;
    let picked = synthesized_playbooks(
        &[high],
        "book a flight ticket to hanoi",
        SYNTH_SKILL_MIN_CONFIDENCE,
        3,
        &SkillGates::default(),
    );
    assert!(picked.is_empty(), "a high-confidence but unrelated skill must not be injected");
}

// ---------------------------------------------------------------------------
// Pipeline Activation phase 5 — skill enable/pin gate enforcement
// ---------------------------------------------------------------------------

#[test]
fn disabled_synthesized_skill_is_excluded_even_at_high_confidence() {
    let mut high = make_skill("s1", "flight-booking", "book a flight ticket for the user");
    high.confidence = 0.95;
    let gates = SkillGates::new(std::collections::HashSet::from(["flight-booking".to_string()]), std::collections::HashSet::new());
    let picked = synthesized_playbooks(
        &[high],
        "please book a flight ticket to hanoi",
        SYNTH_SKILL_MIN_CONFIDENCE,
        3,
        &gates,
    );
    assert!(picked.is_empty(), "a disabled synthesized skill must never be injected");
}

#[test]
fn pinned_synthesized_skill_bypasses_the_confidence_floor_and_match_bar_and_is_ordered_first() {
    // Below the confidence floor AND does not match the task at all — would normally be
    // excluded twice over. Pinning it must surface it anyway, ahead of the genuinely
    // matching skill.
    let mut unrelated_low_conf = make_skill("s1", "weather-lookup", "check the weather forecast for a city");
    unrelated_low_conf.confidence = SYNTH_SKILL_MIN_CONFIDENCE - 0.3;
    let mut matching = make_skill("s2", "flight-booking", "book a flight ticket for the user");
    matching.confidence = SYNTH_SKILL_MIN_CONFIDENCE;

    let gates = SkillGates::new(std::collections::HashSet::new(), std::collections::HashSet::from(["weather-lookup".to_string()]));
    let picked = synthesized_playbooks(
        &[unrelated_low_conf, matching],
        "please book a flight ticket to hanoi",
        SYNTH_SKILL_MIN_CONFIDENCE,
        3,
        &gates,
    );
    assert_eq!(picked.len(), 2);
    assert!(
        picked[0].0.starts_with("weather-lookup"),
        "the pinned skill must be ordered first despite failing both the confidence and match filters: {picked:?}"
    );
}

#[test]
fn pinned_synthesized_skill_stays_bounded_by_top_n() {
    let mut pinned_skill = make_skill("s1", "weather-lookup", "check the weather forecast for a city");
    pinned_skill.confidence = SYNTH_SKILL_MIN_CONFIDENCE;
    let mut matching = make_skill("s2", "flight-booking", "book a flight ticket for the user");
    matching.confidence = SYNTH_SKILL_MIN_CONFIDENCE;

    let gates = SkillGates::new(std::collections::HashSet::new(), std::collections::HashSet::from(["weather-lookup".to_string()]));
    // top_n=1: the pinned skill takes the single slot.
    let picked = synthesized_playbooks(
        &[pinned_skill, matching],
        "please book a flight ticket to hanoi",
        SYNTH_SKILL_MIN_CONFIDENCE,
        1,
        &gates,
    );
    assert_eq!(picked.len(), 1, "pinned entries stay bounded by top_n: {picked:?}");
    assert!(picked[0].0.starts_with("weather-lookup"));
}

// ---------------------------------------------------------------------------
// Jaccard similarity — turn-to-turn repeat-request + skill matching
// ---------------------------------------------------------------------------

#[test]
fn jaccard_similarity_is_1_for_identical_strings() {
    assert_eq!(jaccard_similarity("book a flight to hanoi", "book a flight to hanoi"), 1.0);
}

#[test]
fn jaccard_similarity_is_0_for_disjoint_strings() {
    assert_eq!(jaccard_similarity("book a flight", "water the plants"), 0.0);
}

#[test]
fn is_repeat_request_true_for_near_duplicate_phrasing() {
    // High word overlap — a retry/rephrase of the same ask.
    assert!(is_repeat_request(
        "book a flight to hanoi next week",
        "book a flight to hanoi next weekend"
    ));
}

#[test]
fn is_repeat_request_false_for_an_unrelated_new_topic() {
    assert!(!is_repeat_request(
        "book a flight to hanoi next week",
        "what's the weather like tomorrow"
    ));
}

fn make_skill(id: &str, name: &str, description: &str) -> db_skills::Skill {
    db_skills::Skill {
        id: id.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        pattern: String::new(),
        steps: "[]".to_string(),
        confidence: 0.8,
        use_count: 3,
        last_used_at: None,
        created_at: "2026-07-01T00:00:00+00:00".to_string(),
        updated_at: "2026-07-01T00:00:00+00:00".to_string(),
        deleted_at: None,
        archived_at: None,
    }
}

#[test]
fn find_matching_skill_returns_the_best_scoring_active_skill_above_threshold() {
    let skills = vec![
        make_skill("s1", "flight-booking", "book a flight ticket for the user"),
        make_skill("s2", "weather-lookup", "check the weather forecast for a city"),
    ];
    let found = find_matching_skill("please book a flight ticket to hanoi", &skills)
        .expect("must find a matching skill");
    assert_eq!(found.id, "s1");
}

#[test]
fn find_matching_skill_returns_none_when_nothing_clears_the_bar() {
    let skills = vec![make_skill("s1", "flight-booking", "book a flight ticket for the user")];
    let found = find_matching_skill("what is the capital of france", &skills);
    assert!(
        found.is_none(),
        "an unrelated task must not be force-matched to an unrelated skill"
    );
}

#[test]
fn find_matching_skill_returns_none_for_empty_active_skills() {
    assert!(find_matching_skill("book a flight", &[]).is_none());
}
