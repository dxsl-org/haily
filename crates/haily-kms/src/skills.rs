/// Skill synthesis: clusters similar task traces → LLM generalize → save reusable skills.
/// Also handles EMA confidence updates, exponential decay, and injection screening.
use anyhow::{anyhow, Result};
use haily_db::{queries::skills as db_skills, DbHandle};
use haily_llm::{CompletionRequest, LlmClient, Message};
use std::collections::HashSet;
use tracing::{info, warn};

const MIN_CLUSTER_SIZE: usize = 3;
const CLUSTER_SIMILARITY_THRESHOLD: f32 = 0.40;
const DECAY_LAMBDA: f64 = 0.693 / 24.0; // half-life = 24 h when called hourly
const ARCHIVE_THRESHOLD: f64 = 0.30;
const EMA_ALPHA: f64 = 0.10; // EMA: new = alpha * reward + (1-alpha) * old

/// A skill extracted from traces by the LLM synthesizer.
#[derive(Debug)]
pub struct SynthesizedSkill {
    pub name: String,
    pub description: String,
    pub pattern: String,
    pub steps: Vec<String>,
}

/// Three-way task outcome (phase-08, F22) — replaces the old binary
/// success/failure ("failure" if ANY tool call errored), which made a 9-out-of-10
/// success turn indistinguishable from a total failure in both the stored trace and
/// the EMA reward it drives. Stored as its `AsRef<str>` form in `kms_task_traces.outcome`
/// (schema is unchanged — still a free-text column) so no migration is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    Success,
    Partial,
    Failure,
}

impl TaskOutcome {
    /// `total_calls == 0` (a no-tool-call turn) is `Success` — there is nothing to
    /// have failed. Otherwise: `Failure` when the final response signals inability
    /// OR more than half the tool calls failed; `Partial` when some (but not most)
    /// failed; `Success` otherwise.
    pub fn compute(final_response_signals_inability: bool, failed_calls: usize, total_calls: usize) -> Self {
        if total_calls == 0 {
            return TaskOutcome::Success;
        }
        let failure_ratio = failed_calls as f32 / total_calls as f32;
        if final_response_signals_inability || failure_ratio > 0.5 {
            TaskOutcome::Failure
        } else if failed_calls > 0 {
            TaskOutcome::Partial
        } else {
            TaskOutcome::Success
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            TaskOutcome::Success => "success",
            TaskOutcome::Partial => "partial",
            TaskOutcome::Failure => "failure",
        }
    }

    /// EMA reward this outcome drives: success=1.0, partial=0.5, failure=0.0
    /// (phase-08 spec).
    pub fn ema_reward(&self) -> f64 {
        match self {
            TaskOutcome::Success => 1.0,
            TaskOutcome::Partial => 0.5,
            TaskOutcome::Failure => 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Label provenance (Harness Completion phase 5, researcher-03 §2)
// ---------------------------------------------------------------------------

/// Confidence weights per label source (researcher-03 §2.1): explicit signals and
/// undo are high-precision (≥0.9), repeat-request and tool-error-ratio are weaker
/// correlates. `UNDO_LABEL_CONFIDENCE` is DELIBERATELY near-zero rather than the
/// literature's suggested ≥0.9 — m4's risk note: before local undos populate the
/// journal broadly (which Phase 2 of this same plan enables, already merged on this
/// branch), the signal fires rarely, and calibrating the eval baseline against a
/// near-dead signal at full confidence would be worse than under-weighting it. This
/// is a conscious tuning knob, documented so a future pass can raise it once
/// `undo_within_5min` is observed firing at a healthy rate — not an oversight.
pub const EXPLICIT_FEEDBACK_CONFIDENCE: f64 = 0.9;
/// Phrase-detected (pattern-matched) correction/negative feedback — CAPPED BELOW
/// `EXPLICIT_FEEDBACK_CONFIDENCE` (m2): a parsed phrase is weaker evidence than an
/// explicit `feedback_react` tool call, since a phrase match can misfire on
/// incidental phrasing even after the anchor/short-message precision rules in
/// `feedback_parser`.
pub const PHRASE_FEEDBACK_CONFIDENCE: f64 = 0.5;
pub const TOOL_ERROR_RATIO_CONFIDENCE: f64 = 0.6;
pub const REPEAT_REQUEST_CONFIDENCE: f64 = 0.5;
/// m4: near-zero until Phase 2's local-undo journaling has matured the signal.
pub const UNDO_LABEL_CONFIDENCE: f64 = 0.05;
/// Deterministic pipeline gate outcome (compile/test pass-fail) — Sub-Agent + Skill
/// Architecture phase 8. A verifier gate is a HARD, reproducible signal, so it is weighted
/// high (0.9), just under [`EXPLICIT_FEEDBACK_CONFIDENCE`]: it is strictly better evidence
/// than any phrase heuristic, but a human's explicit reaction always outranks it (LOCKED
/// decision #4). It NEVER overwrites an existing `explicit_feedback` label on a trace — see
/// [`gate_label_supersedes`].
pub const GATE_RESULT_CONFIDENCE: f64 = 0.9;
/// m4 exact predicate window: an `action_journal` row undone within this many
/// minutes of the CURRENT action's `created_at` counts as `undo_within_5min`.
pub const UNDO_WINDOW_MINUTES: i64 = 5;

/// Where a turn's outcome label came from — the anti-reinforcement provenance tag
/// (researcher-03 §2.1). `Unknown` is the safety-critical variant: a turn with no
/// corroborating signal MUST NOT drive `update_skill_confidence` at all (see that
/// function's caller in `haily-core::agent`, which skips the call entirely rather
/// than defaulting to a neutral 0.5 reward).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelSource {
    ExplicitFeedback,
    PhraseFeedback,
    ToolErrorRatio,
    RepeatRequest,
    UndoWithinN,
    /// Deterministic pipeline gate outcome (phase 8) — see [`GATE_RESULT_CONFIDENCE`].
    GateResult,
    Unknown,
}

impl LabelSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            LabelSource::ExplicitFeedback => "explicit_feedback",
            LabelSource::PhraseFeedback => "phrase_feedback",
            LabelSource::ToolErrorRatio => "tool_error_ratio",
            LabelSource::RepeatRequest => "repeat_request",
            LabelSource::UndoWithinN => "undo_within_n_min",
            LabelSource::GateResult => "gate_result",
            LabelSource::Unknown => "unknown",
        }
    }
}

/// Whether a deterministic [`LabelSource::GateResult`] label (phase 8) may overwrite an
/// EXISTING trace label whose `label_source` string is `existing_source`.
///
/// The single anti-reinforcement precedence rule for the gate signal (LOCKED decision #4): a
/// gate result is high-confidence but ALWAYS sits UNDER a human's explicit feedback, so it may
/// replace anything EXCEPT an `explicit_feedback` label. `None` (no prior label) is freely
/// superseded. Kept as one pure predicate so the DB update guard
/// ([`haily_db::queries::skills::apply_gate_result_label`]) and this precedence contract can
/// never drift apart.
pub fn gate_label_supersedes(existing_source: Option<&str>) -> bool {
    existing_source != Some(LabelSource::ExplicitFeedback.as_str())
}

/// A turn's outcome label: provenance + confidence weight. `is_unknown()` is the
/// gate callers must check BEFORE reading `confidence` — there is no meaningful
/// weight for "no signal," and a caller that read `confidence` unconditionally would
/// risk `Unknown`'s placeholder `0.0` being mistaken for "confidently zero reward"
/// rather than "skip this turn entirely."
#[derive(Debug, Clone, Copy)]
pub struct Label {
    pub source: LabelSource,
    pub confidence: f64,
}

impl Label {
    pub fn unknown() -> Self {
        Label {
            source: LabelSource::Unknown,
            confidence: 0.0,
        }
    }

    pub fn is_unknown(&self) -> bool {
        self.source == LabelSource::Unknown
    }
}

/// Derive the label for a turn from the highest-precision signal available, in the
/// priority order researcher-03 §1 ranks by precision: explicit feedback (highest) >
/// undo-within-N > tool-error-ratio > repeat-request > unknown (default, moves
/// nothing). Only ONE signal drives the label even when several could apply — this
/// keeps the EMA input a single well-defined `reward * confidence` rather than an
/// ad-hoc combination the anti-reinforcement guard would need to separately reason
/// about (researcher-03 §2's corroboration-floor guard operates at the skill-archival
/// layer, not here).
///
/// `explicit_feedback`/`phrase_feedback` are NOT computed here — they are joined by
/// the caller (`haily-kms::feedback::apply_feedback_signal`) against the PRIOR
/// turn's trace once a feedback signal fires on THIS turn's message (m2 attribution
/// gate lives at that call site, not in this pure function).
///
/// # M2 review fix — benign repetition must not read as failure
/// `repeat_request` alone is a WEAK, high-noise proxy: a user who habitually sends
/// near-duplicate consecutive messages (e.g. a daily "tóm tắt hôm nay" habit) would
/// otherwise have every one of those turns mislabeled as a failure signal, eroding an
/// otherwise-healthy skill's confidence for behavior that has nothing to do with task
/// quality. `has_corroborating_negative_signal` gates it: an UNCORROBORATED repeat
/// (a clean, all-succeeded turn with no other negative indicator) now stays
/// `unknown` — no confidence movement — and only a repeat ALSO accompanied by a
/// same-turn negative indicator (a `Partial` outcome, i.e. some-but-not-most tool
/// calls failed — `Failure` already wins its own branch above and never reaches
/// here — or an explicit `feedback_react` negative/correction call within this same
/// turn's tool-call log) is labeled `RepeatRequest`. The caller
/// (`haily-core::agent::record_outcome_and_update_skill`) computes this flag.
pub fn derive_label(
    outcome: TaskOutcome,
    undo_within_5min: bool,
    is_repeat_request: bool,
    has_corroborating_negative_signal: bool,
) -> Label {
    if undo_within_5min {
        return Label {
            source: LabelSource::UndoWithinN,
            confidence: UNDO_LABEL_CONFIDENCE,
        };
    }
    if outcome == TaskOutcome::Failure {
        return Label {
            source: LabelSource::ToolErrorRatio,
            confidence: TOOL_ERROR_RATIO_CONFIDENCE,
        };
    }
    if is_repeat_request && has_corroborating_negative_signal {
        return Label {
            source: LabelSource::RepeatRequest,
            confidence: REPEAT_REQUEST_CONFIDENCE,
        };
    }
    Label::unknown()
}

/// Word-overlap similarity between two strings — public wrapper over the same
/// Jaccard function `cluster_traces` uses for hourly clustering, exposed so
/// `haily-core::agent` can reuse it turn-to-turn for repeat-request detection
/// (researcher-03 §1: "next user message is a near-duplicate of the previous
/// task_description" is the single most literature-cited implicit failure signal).
pub fn jaccard_similarity(a: &str, b: &str) -> f32 {
    jaccard(a, b)
}

/// Whether `current` is a near-duplicate of `previous` at the SAME similarity bar
/// hourly clustering uses (`CLUSTER_SIMILARITY_THRESHOLD`) — reusing one constant
/// keeps "similar enough to cluster into a skill" and "similar enough to look like a
/// retry" a single tunable knob rather than two independently-drifting ones.
pub fn is_repeat_request(previous: &str, current: &str) -> bool {
    jaccard(previous, current) >= CLUSTER_SIMILARITY_THRESHOLD
}

/// Find the active skill whose `description` best matches `task_description`, above
/// the clustering similarity bar. This is the seam that lets `update_skill_confidence`
/// (previously dead code — nothing ever called it) target a concrete skill: a turn's
/// trace has no direct skill foreign key (traces predate per-turn skill matching), so
/// the best-effort correlation is textual similarity to what a skill's own
/// `description` says it covers. Returns `None` when no active skill clears the bar —
/// most turns don't correspond to any synthesized skill yet, and that must not be
/// forced into a false match.
pub fn find_matching_skill<'a>(
    task_description: &str,
    active_skills: &'a [db_skills::Skill],
) -> Option<&'a db_skills::Skill> {
    active_skills
        .iter()
        .map(|s| (s, jaccard(task_description, &s.description)))
        .filter(|(_, sim)| *sim >= CLUSTER_SIMILARITY_THRESHOLD)
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(s, _)| s)
}

/// Minimum confidence a SYNTHESIZED skill must clear before it may be injected into a
/// sub-agent's `## Playbooks` pool (Sub-Agent + Skill Architecture phase 8, LOCKED decision:
/// "synthesized skills with confidence ≥0.6 whose pattern matches the task join the ranked
/// pool"). A skill below this bar NEVER reaches the prompt — it is still learning and must not
/// steer a sub-agent yet.
pub const SYNTH_SKILL_MIN_CONFIDENCE: f64 = 0.6;

/// The persisted skill enable/pin admin state (Pipeline Activation phase 5), read ONCE per
/// injection assembly by the caller that already holds `db` (`skill_gates::load`) and passed
/// into the pure selection functions below — [`playbooks_for`](crate::authored_skills::AuthoredRegistry::playbooks_for)
/// and [`synthesized_playbooks`] — so those stay DB-free and unit-testable. Lives here (not in
/// `haily-app`) because BOTH the authored (sync, in-memory) and synthesized (async, DB-backed)
/// selection paths in this crate need it, and `haily-core::agent::sub_turn` (the real injection
/// call site) cannot depend on `haily-app` (reverse of the actual crate layering).
///
/// `Default` (empty sets) reproduces pre-phase-5 behavior exactly — the regression guard the
/// phase's Risk Assessment calls for.
#[derive(Debug, Clone, Default)]
pub struct SkillGates {
    disabled: HashSet<String>,
    pinned: HashSet<String>,
}

impl SkillGates {
    pub fn new(disabled: HashSet<String>, pinned: HashSet<String>) -> Self {
        SkillGates { disabled, pinned }
    }

    /// Whether `name` has an explicit `skill.enabled.<name> = "false"` pref (default is
    /// enabled, so absence is never disabled).
    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled.contains(name)
    }

    /// Whether `name` has an explicit `skill.pinned.<name> = "true"` pref (default is
    /// unpinned).
    pub fn is_pinned(&self, name: &str) -> bool {
        self.pinned.contains(name)
    }
}

/// Select synthesized skills to inject into a sub-turn's `## Playbooks` pool (phase 8).
///
/// Filters out any name `gates` marks disabled, then splits the remainder into pinned and
/// unpinned: a PINNED skill bypasses [`SYNTH_SKILL_MIN_CONFIDENCE`] and the Jaccard match bar
/// entirely (Pipeline Activation phase 5 — the user explicitly asked for it), ranked by (match
/// strength, then confidence) among themselves; the unpinned remainder keeps the original
/// floor + Jaccard-bar filter. Pinned entries are ordered first, then unpinned, both bounded by
/// `top_n` — a pin can surface a skill ahead of the ranked pool but never floods it past budget.
/// Each surviving entry renders as a `(heading, body)` pair with the source made VISIBLE in the
/// heading (`"{name} (synthesized skill)"`) so provenance is never hidden from the model. Pure
/// over its inputs (including `gates`) so the confidence gate and the enable/pin enforcement are
/// both unit-testable without a DB.
pub fn synthesized_playbooks(
    active: &[db_skills::Skill],
    task: &str,
    min_confidence: f64,
    top_n: usize,
    gates: &SkillGates,
) -> Vec<(String, String)> {
    let render = |s: &db_skills::Skill| -> (String, String) {
        let steps: Vec<String> = serde_json::from_str(&s.steps).unwrap_or_default();
        let body = if steps.is_empty() {
            s.description.clone()
        } else {
            format!(
                "{}\nSteps:\n{}",
                s.description,
                steps
                    .iter()
                    .enumerate()
                    .map(|(i, st)| format!("{}. {st}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        (format!("{} (synthesized skill)", s.name), body)
    };

    let candidates: Vec<&db_skills::Skill> = active.iter().filter(|s| !gates.is_disabled(&s.name)).collect();

    let mut pinned: Vec<(f32, f64, &db_skills::Skill)> = candidates
        .iter()
        .copied()
        .filter(|s| gates.is_pinned(&s.name))
        .map(|s| {
            let sim = jaccard(task, &s.description).max(jaccard(task, &s.pattern));
            (sim, s.confidence, s)
        })
        .collect();
    // Descending by match strength, then confidence — a partial_cmp fallback keeps NaN-free
    // f32/f64 comparisons total.
    pinned.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut unpinned: Vec<(f32, f64, &db_skills::Skill)> = candidates
        .iter()
        .copied()
        .filter(|s| !gates.is_pinned(&s.name))
        .filter(|s| s.confidence >= min_confidence)
        .filter_map(|s| {
            // Match against description AND pattern; take the stronger of the two so a skill
            // whose PATTERN captures the task (but whose prose description does not) still qualifies.
            let sim = jaccard(task, &s.description).max(jaccard(task, &s.pattern));
            (sim >= CLUSTER_SIMILARITY_THRESHOLD).then_some((sim, s.confidence, s))
        })
        .collect();
    unpinned.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    pinned
        .into_iter()
        .chain(unpinned)
        .take(top_n)
        .map(|(_, _, s)| render(s))
        .collect()
}

// ---------------------------------------------------------------------------
// Injection screening
// ---------------------------------------------------------------------------

const BLOCKED_PHRASES: &[&str] = &[
    "ignore previous instructions",
    "ignore all instructions",
    "disregard all",
    "system:",
    "<|system|>",
    "javascript:",
    "eval(",
    "exec(",
];

/// Structural bounds enforced by `validate_skill_structure` (F20). These exist
/// independent of the phrase blocklist — a field that is oversized, tag-laden, or
/// carries raw control characters is untrustworthy regardless of whether it happens
/// to contain a currently-known blocked phrase, since the phrase list can never be
/// exhaustive against a creative injection attempt.
const MAX_NAME_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 280;
const MAX_STEP_LEN: usize = 200;

/// Structural validator — runs BEFORE persistence (security boundary: a poisoned
/// skill row should never exist, not merely be screened out at injection time; see
/// phase-08's Security Considerations). Checks, independent of `screen_skill_for_injection`:
///   - `name` ≤ `MAX_NAME_LEN`, `description` ≤ `MAX_DESCRIPTION_LEN` chars
///   - every step ≤ `MAX_STEP_LEN` chars and contains no `<`/`>` (no embedded tags)
///   - every field free of control characters (no allowance — a synthesized field
///     has no legitimate reason to contain any control byte other than the newlines/
///     tabs the LLM might use for its own formatting, which `is_control()` also
///     flags: we deliberately still reject those too, since a skill's structural
///     fields are short single-purpose strings, not free-form prose that needs them)
///   - the same case-folded blocked-phrase scan as `screen_skill_for_injection`,
///     applied per-field so a mismatch localizes to the field that caused it
///
/// Returns the first violation found (not an aggregate) — synthesis already treats
/// any `Err` here as "drop this skill and move to the next cluster" (see
/// `synthesize_skills_from_traces`), so there is no caller that benefits from a full
/// violation list, and returning early keeps this simple (YAGNI).
pub fn validate_skill_structure(skill: &SynthesizedSkill) -> Result<()> {
    if skill.name.chars().count() > MAX_NAME_LEN {
        return Err(anyhow!(
            "skill validation failed: name exceeds {MAX_NAME_LEN} chars"
        ));
    }
    if skill.description.chars().count() > MAX_DESCRIPTION_LEN {
        return Err(anyhow!(
            "skill validation failed: description exceeds {MAX_DESCRIPTION_LEN} chars"
        ));
    }
    for (i, step) in skill.steps.iter().enumerate() {
        if step.chars().count() > MAX_STEP_LEN {
            return Err(anyhow!(
                "skill validation failed: step {i} exceeds {MAX_STEP_LEN} chars"
            ));
        }
        if step.contains('<') || step.contains('>') {
            return Err(anyhow!(
                "skill validation failed: step {i} contains an embedded tag character"
            ));
        }
    }

    let fields: Vec<(&str, &str)> = std::iter::once(("name", skill.name.as_str()))
        .chain(std::iter::once(("description", skill.description.as_str())))
        .chain(std::iter::once(("pattern", skill.pattern.as_str())))
        .chain(skill.steps.iter().map(|s| ("step", s.as_str())))
        .collect();

    for (field_name, value) in &fields {
        if value.chars().any(|c| c.is_control()) {
            return Err(anyhow!(
                "skill validation failed: {field_name} contains a control character"
            ));
        }
        let folded = value.to_lowercase();
        for phrase in BLOCKED_PHRASES {
            if folded.contains(*phrase) {
                return Err(anyhow!(
                    "skill validation failed: {field_name} contains forbidden phrase '{phrase}'"
                ));
            }
        }
    }

    Ok(())
}

/// Injection screening on the WHOLE assembled blob — kept alongside (not replaced
/// by) `validate_skill_structure` as a second pass over the concatenated text: a
/// phrase split across a field boundary in a way `validate_skill_structure`'s
/// per-field scan would miss (e.g. "ignore previous" in `pattern` immediately
/// followed by "instructions" in the first step) is still caught here.
pub fn screen_skill_for_injection(skill: &SynthesizedSkill) -> Result<()> {
    let blob = format!(
        "{} {} {} {}",
        skill.name,
        skill.description,
        skill.pattern,
        skill.steps.join(" ")
    )
    .to_lowercase();

    for phrase in BLOCKED_PHRASES {
        if blob.contains(*phrase) {
            return Err(anyhow!(
                "Skill injection screening failed: forbidden phrase '{phrase}'"
            ));
        }
    }

    if blob.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        return Err(anyhow!("Skill injection screening failed: control characters present"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Clustering (Jaccard word overlap, no embedding required)
// ---------------------------------------------------------------------------

fn jaccard(a: &str, b: &str) -> f32 {
    let wa: HashSet<&str> = a.split_whitespace().collect();
    let wb: HashSet<&str> = b.split_whitespace().collect();
    let inter = wa.intersection(&wb).count();
    let union = wa.union(&wb).count();
    if union == 0 { 0.0 } else { inter as f32 / union as f32 }
}

fn cluster_traces(tasks: &[String]) -> Vec<Vec<usize>> {
    let n = tasks.len();
    let mut assigned = vec![false; n];
    let mut clusters: Vec<Vec<usize>> = Vec::new();

    for i in 0..n {
        if assigned[i] { continue; }
        let mut c = vec![i];
        assigned[i] = true;
        for j in (i + 1)..n {
            if !assigned[j] && jaccard(&tasks[i], &tasks[j]) >= CLUSTER_SIMILARITY_THRESHOLD {
                c.push(j);
                assigned[j] = true;
            }
        }
        if c.len() >= MIN_CLUSTER_SIZE {
            clusters.push(c);
        }
    }
    clusters
}

// ---------------------------------------------------------------------------
// LLM-based synthesis
// ---------------------------------------------------------------------------

/// Strip control chars, angle brackets, and excess length from a user-supplied trace description
/// before interpolating into the LLM synthesis prompt (M4 / ASI01).
fn sanitize_trace_desc(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(200)
        .map(|c| match c { '<' | '>' => ' ', c => c })
        .collect()
}

fn synthesis_prompt(trace_descs: &[&str]) -> String {
    let safe_descs: Vec<String> = trace_descs.iter().map(|d| sanitize_trace_desc(d)).collect();
    format!(
        "---TASK---\n\
         Các traces sau đây thực hiện cùng loại task. \
         Hãy generalize thành 1 reusable skill và trả về JSON hợp lệ (không markdown):\n\
         {{\"name\":\"...\",\"description\":\"...\",\"pattern\":\"...\",\"steps\":[...]}}\n\
         ---TRACES---\n{}\n---END---",
        safe_descs.join("\n")
    )
}

fn parse_llm_skill(raw: &str) -> Option<SynthesizedSkill> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    let json_str = &raw[start..=end];
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;

    Some(SynthesizedSkill {
        name: v["name"].as_str()?.to_string(),
        description: v["description"].as_str()?.to_string(),
        pattern: v["pattern"].as_str()?.to_string(),
        steps: v["steps"]
            .as_array()?
            .iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect(),
    })
}

// ---------------------------------------------------------------------------
// Public API (consumed by KmsHandle methods)
// ---------------------------------------------------------------------------

/// Load recent traces, cluster by similarity, ask LLM to generalize each cluster,
/// screen for injection, and persist new skills. Returns the list of skills saved.
pub async fn synthesize_skills_from_traces(
    db: &DbHandle,
    llm: &dyn LlmClient,
) -> Result<Vec<db_skills::Skill>> {
    let one_hour_ago = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let traces = db_skills::traces_since(db, &one_hour_ago).await?;
    if traces.len() < MIN_CLUSTER_SIZE {
        return Ok(vec![]);
    }

    let descriptions: Vec<String> = traces.iter().map(|t| t.task_description.clone()).collect();
    let clusters = cluster_traces(&descriptions);

    let mut saved: Vec<db_skills::Skill> = Vec::new();

    for cluster in clusters {
        let descs: Vec<&str> = cluster.iter().map(|&i| descriptions[i].as_str()).collect();
        let prompt = synthesis_prompt(&descs);

        let req = CompletionRequest::simple(vec![
            Message::system(
                "You are a skill synthesizer. Respond ONLY with valid JSON, no markdown.",
            ),
            Message::user(prompt),
        ]);

        let raw = match llm.complete(req).await {
            Ok(r) => r,
            Err(e) => {
                warn!("skill synthesis LLM call failed: {e:#}");
                continue;
            }
        };

        let skill = match parse_llm_skill(&raw) {
            Some(s) => s,
            None => {
                warn!("skill synthesis: could not parse LLM output");
                continue;
            }
        };

        // Structural validation runs BEFORE the phrase-based injection screen and
        // BEFORE persistence (F20 / phase-08 Security Considerations: a poisoned
        // skill row must never exist, not merely be screened out at injection time).
        if let Err(e) = validate_skill_structure(&skill) {
            warn!("skill rejected by structural validator: {e:#}");
            continue;
        }
        if let Err(e) = screen_skill_for_injection(&skill) {
            warn!("skill rejected by injection screen: {e:#}");
            continue;
        }

        let steps_json = serde_json::to_string(&skill.steps).unwrap_or_default();
        match db_skills::insert_skill(db, &skill.name, &skill.description, &skill.pattern, &steps_json).await {
            Ok(s) => {
                info!(name = %s.name, "skill synthesized and saved");
                saved.push(s);
            }
            Err(e) => warn!("skill insert failed: {e:#}"),
        }
    }

    Ok(saved)
}

/// EMA confidence update after a tool outcome.
/// `reward` = 1.0 for success, 0.0 for failure.
///
/// The EMA math runs inside a single atomic SQL UPDATE (`db_skills::update_skill_confidence`)
/// rather than a Rust-side read-modify-write, so two concurrent updates for the same skill
/// both apply instead of one clobbering the other.
pub async fn update_skill_confidence(db: &DbHandle, skill_id: &str, reward: f64) -> Result<()> {
    db_skills::update_skill_confidence(db, skill_id, reward, EMA_ALPHA).await
}

/// Corroboration floor (Harness Completion phase 5, M1 review fix — researcher-03 §2.3
/// "corroboration floor before high-confidence auto-action"): a skill must have at
/// least this many recent negative-labeled traces matched to it before decay is
/// allowed to archive it, mirroring Haily's own prior red-teamed conclusion (memory
/// 2026-06-21 project-memory-anti-reinforcement-plan) that the SAME weak signal must
/// not both produce a low confidence AND validate the decision to act on it.
const MIN_CORROBORATING_NEGATIVES: usize = 2;

/// How far back `apply_skill_decay` looks for negative-labeled traces when checking
/// corroboration — generous enough that a skill used only occasionally still has a
/// fair chance to accumulate corroborating evidence before this cycle's decay would
/// otherwise archive it outright.
const CORROBORATION_LOOKBACK_DAYS: i64 = 30;

/// Whether at least `MIN_CORROBORATING_NEGATIVES` negative-labeled traces (each
/// matched to `skill` by the SAME Jaccard bar `find_matching_skill` uses) exist in
/// `negative_traces`. Per the "at minimum" fallback in the phase's Risk Notes
/// (distinct `label_source` values are a STRONGER bar than merely 2 negative traces,
/// preferred when available but not required): this counts DISTINCT `label_source`
/// values among the matches when at least 2 are present, but also accepts 2+ matches
/// sharing one `label_source` (e.g. two separate `tool_error_ratio` turns) as the
/// minimum viable corroboration — a single skill failing the exact same way twice on
/// two DIFFERENT turns is still independent evidence, not a single restated signal.
fn is_archival_corroborated(skill: &db_skills::Skill, negative_traces: &[db_skills::TaskTrace]) -> bool {
    let matches: Vec<&db_skills::TaskTrace> = negative_traces
        .iter()
        .filter(|t| jaccard(&skill.description, &t.task_description) >= CLUSTER_SIMILARITY_THRESHOLD)
        .collect();

    if matches.len() < MIN_CORROBORATING_NEGATIVES {
        return false;
    }

    // Prefer DISTINCT label_source diversity when it's present (stronger evidence:
    // two independently-arrived-at negative signals, not the same detector firing
    // twice), but do not require it — 2 traces alone already clears the floor above.
    let distinct_sources: HashSet<&str> = matches
        .iter()
        .filter_map(|t| t.label_source.as_deref())
        .collect();
    if distinct_sources.len() >= MIN_CORROBORATING_NEGATIVES {
        return true;
    }

    // Falls through to the "at minimum 2 independent negative-labeled traces on
    // different turns" floor — `matches.len() >= MIN_CORROBORATING_NEGATIVES` was
    // already confirmed above, and each element is a distinct trace row (a distinct
    // turn) by construction of `negative_traces_since`.
    true
}

/// Apply exponential decay to all active skills' confidence, then archive ONLY those
/// whose decayed confidence falls below `ARCHIVE_THRESHOLD` AND clear the
/// corroboration floor (`is_archival_corroborated`) — Should be called every 24 hours.
///
/// A skill that crosses below the threshold WITHOUT corroboration is deliberately
/// left active at its (low) decayed confidence rather than archived: "hold at the
/// floor" per the phase's M1 fix, so a skill is never removed purely because decay
/// alone pushed a stale-but-unrefuted skill under the line — the same anti-
/// reinforcement principle `derive_label`'s `unknown`-never-moves-confidence
/// invariant applies to the EMA now also gates the archival action.
pub async fn apply_skill_decay(db: &DbHandle) -> Result<()> {
    let touched = db_skills::apply_exponential_decay(db, DECAY_LAMBDA).await?;
    if touched == 0 {
        return Ok(());
    }

    let candidates = db_skills::skills_below_confidence(db, ARCHIVE_THRESHOLD).await?;
    if candidates.is_empty() {
        return Ok(());
    }

    let since = (chrono::Utc::now() - chrono::Duration::days(CORROBORATION_LOOKBACK_DAYS)).to_rfc3339();
    let negative_traces = db_skills::negative_traces_since(db, &since).await?;

    let mut archived = 0usize;
    let mut held = 0usize;
    for skill in &candidates {
        if is_archival_corroborated(skill, &negative_traces) {
            db_skills::archive_skill(db, &skill.id).await?;
            archived += 1;
        } else {
            held += 1;
        }
    }

    if archived > 0 {
        info!(count = archived, "skills archived by decay (corroborated)");
    }
    if held > 0 {
        info!(
            count = held,
            "skills held at low confidence — decay threshold crossed WITHOUT corroboration, archival withheld"
        );
    }
    Ok(())
}
