/// Hybrid semantic+keyword search over KMS facts.
///
/// Strategy:
/// - FTS5 BM25: always runs (text keyword match)
/// - HNSW ANN: runs when a query embedding is provided (semantic cosine similarity)
/// - Results are merged by fact ID; facts found by both paths get a score boost.
/// - Final list is sorted by score descending, ties broken by recency, then truncated
///   to `limit`.
///
/// ## Measure-first relevance (Phase "assistant-depth" #1)
/// The fused score sums two INCOMMENSURABLE signals: ANN's `1 - cosine_dist` is a
/// real relevance value, while FTS's old rank-position decay was NOT — a lone weak
/// lexical match ranked #0 (nothing else matched) scored ~1.0 either way. A single
/// fused floor cannot tell "relevant" apart from "merely top-of-list", so relevance
/// is thresholded PER CHANNEL, before fusion, on each channel's own real signal:
/// - ANN: the raw cosine distance (`ANN_DIST_MAX`).
/// - FTS: the actual SQLite `bm25()` value (`BM25_CUTOFF`), replacing the old
///   rank-position decay as the FTS relevance basis (see `fts_relevance_score`).
///
/// Both consts default to `None` (no threshold — today's behavior, unchanged) until
/// `crates/haily-kms/tests/recall_relevance.rs` measures a distribution that
/// justifies a concrete cutoff. Guessing a floor risks Haily silently "forgetting"
/// facts that a too-aggressive constant drops; shipping no floor risks nothing beyond
/// today's already-shipped behavior. See that test file for the measurement status.
use anyhow::Result;
use haily_db::{queries::facts, DbHandle};

use crate::hnsw::HnswIndex;

/// ANN cosine-distance cutoff (0.0 = identical, 1.0 = orthogonal; can exceed 1.0 for
/// obtuse angles but HNSW rarely surfaces those). `None` = no threshold (current
/// default). Promote to `Some(x)` only after `recall_relevance.rs`'s ANN measurement
/// (gated behind `--features embeddings`) shows a clean on-topic/off-topic
/// separation — see that file's module doc for the current measurement status.
const ANN_DIST_MAX: Option<f32> = None;

/// Absolute SQLite `bm25()` cutoff. `bm25()` returns MORE-NEGATIVE values for BETTER
/// matches (confirmed by `recall_relevance.rs`'s bm25 measurement, opposite of
/// "higher is better" elsewhere in this codebase) — so this is compared as
/// `bm25_score > BM25_CUTOFF` ⇒ drop (i.e. `BM25_CUTOFF` is the least-negative score
/// still considered a real match). `None` = no threshold (current default). Promote
/// to `Some(x)` only after measuring the real corpus distribution.
const BM25_CUTOFF: Option<f64> = None;

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub id: String,
    /// Human-readable "subject predicate object" for context injection.
    pub text: String,
    /// 0.0–2.0; higher is more relevant. BM25 base < 1.0, ANN base = 1.0.
    pub score: f32,
    pub source: SearchSource,
    /// RFC3339 `kms_facts.updated_at` — the recency tie-break key when two results
    /// have equal `score` (most-recently-updated wins).
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SearchSource {
    Fts,
    Hnsw,
    Both,
}

/// Squash an unbounded `bm25()` "goodness" value into `[0, 1)` so it stays
/// commensurable with the ANN channel's bounded `1 - cosine_dist` score for the
/// existing "found by both channels" boost and for sorting — the cutoff test itself
/// (`BM25_CUTOFF`) still compares against the raw, un-squashed score, not this value.
/// `bm25_score <= 0.0` for any real FTS5 match; a non-negative input (no match, or a
/// pathological corpus) maps to 0.0 goodness rather than a negative score.
fn fts_relevance_score(bm25_score: f64) -> f32 {
    let goodness = (-bm25_score).max(0.0) as f32;
    goodness / (1.0 + goodness)
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
    let fts_hits = facts::search_fts(db, query, fts_limit).await?;

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

    for hit in &fts_hits {
        // MEASURE-FIRST: only filters once `BM25_CUTOFF` is `Some` (see const doc).
        if let Some(cutoff) = BM25_CUTOFF {
            if hit.bm25_score > cutoff {
                continue;
            }
        }
        let fts_score = fts_relevance_score(hit.bm25_score);
        scores.insert(hit.fact.id.clone(), (fts_score, SearchSource::Fts));
    }

    for (id, dist) in &ann_results {
        // dist is cosine distance (0 = identical, 1 = orthogonal). MEASURE-FIRST:
        // only filters once `ANN_DIST_MAX` is `Some` (see const doc).
        if let Some(max_dist) = ANN_DIST_MAX {
            if *dist > max_dist {
                continue;
            }
        }
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
        fts_hits.iter().map(|h| h.fact.id.as_str()).collect();

    let mut results: Vec<SearchResult> = Vec::with_capacity(scores.len());

    // Add FTS facts that survived the cutoff (skipped ones are absent from `scores`)
    for hit in &fts_hits {
        if let Some((score, source)) = scores.get(&hit.fact.id) {
            results.push(SearchResult {
                id: hit.fact.id.clone(),
                text: format!("{} {} {}", hit.fact.subject, hit.fact.predicate, hit.fact.object),
                score: *score,
                source: source.clone(),
                updated_at: hit.fact.updated_at.clone(),
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
                    updated_at: fact.updated_at.clone(),
                });
            }
        }
    }

    // Sort by score descending; equal scores break toward the more-recently-updated
    // fact (RFC3339 strings sort lexically in chronological order).
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    results.truncate(limit);
    Ok(results)
}
