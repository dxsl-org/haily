pub mod feedback;
pub mod hnsw;
pub mod search;
pub mod skills;
pub mod system_prompt;

#[cfg(feature = "embeddings")]
pub mod embedder;

use anyhow::Result;
use haily_db::{queries::facts, queries::meta, queries::skills as db_skills, DbHandle};
use std::sync::Arc;
use uuid::Uuid;

#[cfg(feature = "embeddings")]
use embedder::Embedder;

use hnsw::HnswIndex;

pub struct KmsHandle {
    pub(crate) db: DbHandle,
    hnsw: Arc<HnswIndex>,
    #[cfg(feature = "embeddings")]
    embedder: Arc<Embedder>,
}

impl KmsHandle {
    /// Initialise KMS: build HNSW index from persisted embeddings.
    /// With `embeddings` feature: also init fastembed model (downloads ~150 MB on first run).
    pub async fn init(db: DbHandle) -> Result<Self> {
        let hnsw = Arc::new(HnswIndex::new());

        // Load blobs from DB and populate HNSW (works without embeddings feature too —
        // blobs are just stored LE f32 arrays regardless of how they were generated)
        let rows = facts::embeddings_for_hnsw(&db).await?;
        if !rows.is_empty() {
            let hnsw_clone = Arc::clone(&hnsw);
            tokio::task::spawn_blocking(move || {
                let items: Vec<(String, Vec<f32>)> = rows
                    .into_iter()
                    .map(|(id, blob)| {
                        let floats = blob
                            .chunks_exact(4)
                            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                            .collect();
                        (id, floats)
                    })
                    .collect();
                hnsw_clone.batch_insert(&items);
            })
            .await?;
            tracing::info!(count = hnsw.len(), "HNSW index rebuilt from DB");
        }

        #[cfg(feature = "embeddings")]
        let embedder = {
            let emb = tokio::task::spawn_blocking(Embedder::init).await??;
            Arc::new(emb)
        };

        Ok(Self {
            db,
            hnsw,
            #[cfg(feature = "embeddings")]
            embedder,
        })
    }

    /// Hybrid search: FTS5 BM25 always; HNSW ANN when embeddings feature is active.
    /// Returns a ranked list of fact texts relevant to `query`.
    pub async fn search_hybrid(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<search::SearchResult>> {
        #[cfg(feature = "embeddings")]
        {
            let embedder = Arc::clone(&self.embedder);
            let query_owned = query.to_string();
            let qv = tokio::task::spawn_blocking(move || embedder.embed_query(&query_owned))
                .await??;
            search::hybrid(&self.db, &self.hnsw, Some(&qv), query, limit).await
        }
        #[cfg(not(feature = "embeddings"))]
        {
            search::hybrid(&self.db, &self.hnsw, None, query, limit).await
        }
    }

    /// Insert a fact and update the HNSW index in-place.
    /// If `text` is provided (subject+predicate+object joined), it is embedded and stored.
    pub async fn remember(
        &self,
        domain_id: &str,
        subject: &str,
        predicate: &str,
        object: &str,
        source: &str,
        source_ref: Option<&str>,
    ) -> Result<String> {
        #[cfg(feature = "embeddings")]
        {
            let text = format!("{subject} {predicate} {object}");
            let embedder = Arc::clone(&self.embedder);
            let embedding = tokio::task::spawn_blocking(move || {
                embedder.embed_passages(&[text])
            })
            .await??
            .into_iter()
            .next()
            .unwrap_or_default();

            let blob = Embedder::to_bytes(&embedding);
            let fact = facts::insert_fact(
                &self.db,
                facts::NewFact {
                    domain_id,
                    subject,
                    predicate,
                    object,
                    source,
                    source_ref,
                    embedding: Some(&blob),
                },
            )
            .await?;

            self.hnsw.insert(&fact.id, &embedding);
            Ok(fact.id)
        }
        #[cfg(not(feature = "embeddings"))]
        {
            let fact = facts::insert_fact(
                &self.db,
                facts::NewFact {
                    domain_id,
                    subject,
                    predicate,
                    object,
                    source,
                    source_ref,
                    embedding: None,
                },
            )
            .await?;
            Ok(fact.id)
        }
    }

    /// Build a LifeContext snapshot for a session.
    /// Loads agent identity, feedback preferences, corrections, and top active skills.
    pub async fn build_life_context(&self, session_id: Uuid) -> Result<LifeContext> {
        let _ = session_id; // will be used in Phase 07 for per-session soul overrides

        let agent_name = meta::get_preference(&self.db, "agent.name")
            .await?
            .unwrap_or_else(|| "Haily".to_string());

        let soul_str = meta::get_preference(&self.db, "agent.soul")
            .await?
            .unwrap_or_else(|| "haily".to_string());

        let soul = Soul::from_name(&soul_str);

        let user_address = meta::get_preference(&self.db, "user.address")
            .await?
            .unwrap_or_else(|| "bạn".to_string());

        let agent_pronoun = meta::get_preference(&self.db, "agent.pronoun")
            .await?
            .unwrap_or_else(|| "tôi".to_string());

        // Build feedback directives from stored preferences (C1 — close the feedback loop).
        let mut feedback_directives: Vec<String> = Vec::new();

        if meta::get_preference(&self.db, "prefer_shorter_responses").await?.as_deref() == Some("true") {
            feedback_directives.push("Trả lời ngắn gọn, súc tích.".to_string());
        }
        if meta::get_preference(&self.db, "feedback.language_complaint").await?.is_some() {
            feedback_directives.push("Chú ý dùng đúng ngôn ngữ mà người dùng yêu cầu.".to_string());
        }
        if meta::get_preference(&self.db, "feedback.tone_complaint").await?.is_some() {
            feedback_directives.push("Điều chỉnh phong cách theo phản hồi của người dùng.".to_string());
        }
        for pref in meta::list_by_prefix(&self.db, "feedback.correction.").await? {
            let old = pref.key
                .trim_start_matches("feedback.correction.")
                .replace('_', " ");
            feedback_directives.push(format!("Sửa: \"{}\" → \"{}\"", old, pref.value));
        }

        // Load top-5 active skills (C2 — inject synthesized skills into context).
        let skill_rows = db_skills::active_skills_top(&self.db, 5).await?;
        let active_skills: Vec<SkillSummary> = skill_rows
            .into_iter()
            .map(|s| SkillSummary {
                name: s.name,
                description: s.description,
                pattern: s.pattern,
            })
            .collect();

        Ok(LifeContext {
            agent_name,
            soul,
            user_address,
            agent_pronoun,
            relevant_facts: vec![],
            feedback_directives,
            active_skills,
        })
    }

    /// Build a system prompt string for the given LifeContext.
    pub fn build_system_prompt(&self, ctx: &LifeContext) -> String {
        system_prompt::build(ctx)
    }

    /// Synthesize reusable skills from recent task traces (Phase 11).
    pub async fn synthesize_skills(
        &self,
        llm: &dyn haily_llm::LlmClient,
    ) -> Result<Vec<haily_db::queries::skills::Skill>> {
        skills::synthesize_skills_from_traces(&self.db, llm).await
    }

    /// Apply exponential confidence decay to all skills (Phase 11, every 24 h).
    pub async fn decay_skills(&self) -> Result<()> {
        skills::apply_skill_decay(&self.db).await
    }

    pub fn db(&self) -> &DbHandle {
        &self.db
    }
}

#[derive(Debug, Clone)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub pattern: String,
}

#[derive(Debug, Clone)]
pub struct LifeContext {
    pub agent_name: String,
    pub soul: Soul,
    pub user_address: String,
    pub agent_pronoun: String,
    /// Fact texts (subject predicate object) injected as memory bullets.
    pub relevant_facts: Vec<String>,
    /// Short directives derived from user feedback preferences.
    pub feedback_directives: Vec<String>,
    /// Top active skills to guide the LLM toward learned patterns.
    pub active_skills: Vec<SkillSummary>,
}

#[derive(Debug, Clone, Default)]
pub enum Soul {
    #[default]
    Haily,
    Tete,
    Hoami,
    Lungmat,
}

impl Soul {
    /// Parse a soul from its Vietnamese or ASCII name. Infallible — unknown
    /// names fall back to `Soul::Haily`, so this deliberately does not implement
    /// `std::str::FromStr` (which would force a meaningless error type).
    pub fn from_name(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "tete" | "tê tê" => Soul::Tete,
            "hoami" | "họa mi" => Soul::Hoami,
            "lungmat" | "lửng mật" => Soul::Lungmat,
            _ => Soul::Haily,
        }
    }
}
