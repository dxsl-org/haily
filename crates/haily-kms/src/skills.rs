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

/// Apply exponential decay to all active skills. Archive any whose confidence
/// drops below ARCHIVE_THRESHOLD. Should be called every 24 hours.
pub async fn apply_skill_decay(db: &DbHandle) -> Result<()> {
    let archived = db_skills::apply_exponential_decay(db, DECAY_LAMBDA, ARCHIVE_THRESHOLD).await?;
    if archived > 0 {
        info!(count = archived, "skills archived by decay");
    }
    Ok(())
}
