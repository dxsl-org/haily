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

    let ctrl = blob.chars().filter(|c| c.is_control() && *c != '\n' && *c != '\t').count();
    if ctrl > 5 {
        return Err(anyhow!("Skill injection screening failed: excessive control characters"));
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

fn synthesis_prompt(trace_descs: &[&str]) -> String {
    format!(
        "Các traces sau đây thực hiện cùng loại task. \
         Hãy generalize thành 1 reusable skill và trả về JSON hợp lệ (không markdown):\n\
         {{\"name\":\"...\",\"description\":\"...\",\"pattern\":\"...\",\"steps\":[...]}}\n\n\
         Traces:\n{}",
        trace_descs.join("\n")
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
pub async fn update_skill_confidence(db: &DbHandle, skill_id: &str, reward: f64) -> Result<()> {
    let skills = db_skills::active_skills(db).await?;
    let skill = skills.iter().find(|s| s.id == skill_id);
    if let Some(s) = skill {
        let new_conf = EMA_ALPHA * reward + (1.0 - EMA_ALPHA) * s.confidence;
        db_skills::update_skill_confidence(db, skill_id, new_conf.clamp(0.0, 1.0)).await?;
    }
    Ok(())
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
