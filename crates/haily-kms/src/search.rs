/// Hybrid semantic+keyword search over KMS facts.
///
/// Strategy:
/// - FTS5 BM25: always runs (text keyword match)
/// - HNSW ANN: runs when a query embedding is provided (semantic cosine similarity)
/// - Results are merged by fact ID; facts found by both paths get a score boost.
/// - Final list is sorted by score descending and truncated to `limit`.
use anyhow::Result;
use haily_db::{queries::facts, DbHandle};

use crate::hnsw::HnswIndex;

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub id: String,
    /// Human-readable "subject predicate object" for context injection.
    pub text: String,
    /// 0.0–2.0; higher is more relevant. BM25 base = 1.0, ANN base = 1.0.
    pub score: f32,
    pub source: SearchSource,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SearchSource {
    Fts,
    Hnsw,
    Both,
}

/// Merge BM25 and optional ANN results into a ranked, deduplicated list.
///
/// `query_embedding`: if None, only the FTS5 path runs.
/// `limit`: max results returned.
pub async fn hybrid(
    db: &DbHandle,
    hnsw: &HnswIndex,
    query_embedding: Option<&[f32]>,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    use std::collections::HashMap;

    // BM25 path — always runs
    let fts_limit = (limit * 2) as i64;
    let fts_facts = facts::search_fts(db, query, fts_limit).await?;

    // ANN path — only when embedding is provided and index has entries
    let ann_results: Vec<(String, f32)> = if let Some(qv) = query_embedding {
        if !hnsw.is_empty() {
            hnsw.search(qv, limit * 2)
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    // Build score map keyed by fact id
    let mut scores: HashMap<String, (f32, SearchSource)> = HashMap::new();

    for (rank, fact) in fts_facts.iter().enumerate() {
        // Decay BM25 score with rank: top hit = 1.0, linear decay
        let fts_score = 1.0 - (rank as f32 / (fts_limit as f32 * 2.0));
        scores.insert(fact.id.clone(), (fts_score.max(0.0), SearchSource::Fts));
    }

    for (id, dist) in &ann_results {
        // dist is cosine distance (0 = identical, 1 = orthogonal)
        let ann_score = 1.0 - dist.clamp(0.0, 1.0);
        scores
            .entry(id.clone())
            .and_modify(|(existing_score, source)| {
                *existing_score += ann_score; // boost: found by both = up to 2.0
                *source = SearchSource::Both;
            })
            .or_insert((ann_score, SearchSource::Hnsw));
    }

    // Collect fact texts for all scored ids
    // FTS facts already loaded; fetch any ANN-only facts from DB
    let fts_ids: std::collections::HashSet<&str> =
        fts_facts.iter().map(|f| f.id.as_str()).collect();

    let mut results: Vec<SearchResult> = Vec::with_capacity(scores.len());

    // Add FTS facts
    for fact in &fts_facts {
        if let Some((score, source)) = scores.get(&fact.id) {
            results.push(SearchResult {
                id: fact.id.clone(),
                text: format!("{} {} {}", fact.subject, fact.predicate, fact.object),
                score: *score,
                source: source.clone(),
            });
        }
    }

    // Add ANN-only facts (fetch individually from DB)
    for (id, (score, source)) in &scores {
        if *source == SearchSource::Hnsw && !fts_ids.contains(id.as_str()) {
            if let Some(fact) = facts::get_fact(db, id).await? {
                results.push(SearchResult {
                    id: fact.id.clone(),
                    text: format!("{} {} {}", fact.subject, fact.predicate, fact.object),
                    score: *score,
                    source: source.clone(),
                });
            }
        }
    }

    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);
    Ok(results)
}
