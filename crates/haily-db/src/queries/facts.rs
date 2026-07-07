use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Fact {
    pub id: String,
    pub domain_id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f64,
    pub source: String,
    pub source_ref: Option<String>,
    pub embedding: Option<Vec<u8>>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
    pub archived_at: Option<String>,
}

/// Fields for creating a fact. Grouped into a struct to keep `insert_fact`
/// within a sane arity and to make call sites self-documenting.
pub struct NewFact<'a> {
    pub domain_id: &'a str,
    pub subject: &'a str,
    pub predicate: &'a str,
    pub object: &'a str,
    pub source: &'a str,
    pub source_ref: Option<&'a str>,
    pub embedding: Option<&'a [u8]>,
}

pub async fn insert_fact(db: &DbHandle, fact: NewFact<'_>) -> Result<Fact> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, Fact>(
        "INSERT INTO kms_facts (id, domain_id, subject, predicate, object,
             confidence, source, source_ref, embedding, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 1.0, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(fact.domain_id)
    .bind(fact.subject)
    .bind(fact.predicate)
    .bind(fact.object)
    .bind(fact.source)
    .bind(fact.source_ref)
    .bind(fact.embedding)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

pub async fn get_fact(db: &DbHandle, id: &str) -> Result<Option<Fact>> {
    Ok(sqlx::query_as::<_, Fact>(
        "SELECT * FROM kms_facts WHERE id = ? AND deleted_at IS NULL AND archived_at IS NULL",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await?)
}

/// A fact plus the raw SQLite `bm25()` score that produced its FTS5 match.
///
/// The score is carried out separately from `Fact` (rather than added as a field on
/// `Fact` itself) because it is meaningless outside an FTS query result — `Fact` is
/// also returned by `get_fact`/`list_top`/etc. where no bm25 score exists. `#[sqlx(
/// flatten)]` maps the `f.*` columns onto `Fact` while `bm25_score` binds to the
/// query's extra `AS bm25_score` column, in one `query_as` call.
#[derive(Debug, Clone, FromRow)]
pub struct FtsHit {
    #[sqlx(flatten)]
    pub fact: Fact,
    /// SQLite FTS5 `bm25()` value. MORE NEGATIVE means a BETTER match — this is the
    /// opposite of "higher is better" scores elsewhere in the codebase, so callers
    /// must not treat this like `SearchResult::score`. See `haily-kms/src/search.rs`
    /// (`BM25_CUTOFF`) for the consumer that applies an absolute cutoff on this value.
    pub bm25_score: f64,
}

/// FTS5 BM25 search. Returns most relevant facts first, each paired with the actual
/// `bm25()` value (not just its rank position) so callers can apply an absolute
/// relevance cutoff instead of a rank-position proxy — a lone weak match ranked #0
/// is NOT the same as a strong match, and only the real score can tell them apart.
pub async fn search_fts(db: &DbHandle, query: &str, limit: i64) -> Result<Vec<FtsHit>> {
    Ok(sqlx::query_as::<_, FtsHit>(
        "SELECT f.*, bm25(facts_fts) AS bm25_score FROM kms_facts f
         JOIN facts_fts ON f.rowid = facts_fts.rowid
         WHERE facts_fts MATCH ?
           AND f.deleted_at IS NULL
           AND f.archived_at IS NULL
         ORDER BY bm25_score LIMIT ?",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(db.pool())
    .await?)
}

/// Returns (id, embedding_blob) for all facts with embeddings — used at startup
/// to rebuild the in-memory HNSW index in haily-kms.
pub async fn embeddings_for_hnsw(db: &DbHandle) -> Result<Vec<(String, Vec<u8>)>> {
    let rows = sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT id, embedding FROM kms_facts
         WHERE embedding IS NOT NULL AND deleted_at IS NULL AND archived_at IS NULL",
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows)
}

/// Facts with embeddings created strictly after `since` (RFC3339) — the delta insert
/// half of HNSW dump/load reconciliation. `created_at` (not `updated_at`) is the
/// right anchor here: a fact edited-in-place after the dump but created before it is
/// still findable by ANN (its embedding blob and id are unchanged), whereas a fact
/// created after the dump has never been inserted into the loaded graph at all.
pub async fn embeddings_created_since(
    db: &DbHandle,
    since: &str,
) -> Result<Vec<(String, Vec<u8>)>> {
    let rows = sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT id, embedding FROM kms_facts
         WHERE embedding IS NOT NULL AND deleted_at IS NULL AND archived_at IS NULL
           AND created_at > ?",
    )
    .bind(since)
    .fetch_all(db.pool())
    .await?;
    Ok(rows)
}

/// Fact ids soft-deleted or archived strictly after `since` (RFC3339) — the tombstone
/// half of HNSW dump/load reconciliation, for facts that were live when the dump was
/// taken but must not surface from the loaded (stale) graph.
pub async fn ids_deleted_or_archived_since(db: &DbHandle, since: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM kms_facts
         WHERE (deleted_at IS NOT NULL AND deleted_at > ?)
            OR (archived_at IS NOT NULL AND archived_at > ?)",
    )
    .bind(since)
    .bind(since)
    .fetch_all(db.pool())
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Archive a fact directly (distinct from the EMA-driven `update_confidence` path) —
/// used by explicit archival flows. Idempotent: archiving an already-archived or
/// soft-deleted fact affects 0 rows and returns `false`.
pub async fn archive(db: &DbHandle, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE kms_facts SET archived_at = ?, updated_at = ?
         WHERE id = ? AND deleted_at IS NULL AND archived_at IS NULL",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}

pub async fn list_by_domain(db: &DbHandle, domain_id: &str, limit: i64) -> Result<Vec<Fact>> {
    Ok(sqlx::query_as::<_, Fact>(
        "SELECT * FROM kms_facts WHERE domain_id = ? AND deleted_at IS NULL AND archived_at IS NULL ORDER BY created_at DESC LIMIT ?"
    )
    .bind(domain_id).bind(limit)
    .fetch_all(db.pool()).await?)
}

pub async fn list_top(db: &DbHandle, limit: i64) -> Result<Vec<Fact>> {
    Ok(sqlx::query_as::<_, Fact>(
        "SELECT * FROM kms_facts WHERE deleted_at IS NULL AND archived_at IS NULL ORDER BY confidence DESC LIMIT ?"
    )
    .bind(limit)
    .fetch_all(db.pool()).await?)
}

pub async fn soft_delete(db: &DbHandle, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE kms_facts SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// Fetch a fact's raw `updated_at`, REGARDLESS of `deleted_at`/`archived_at` — unlike
/// `get_fact`'s live-row filter, which would hide a just-soft-deleted row. Used to
/// capture the C10 undo-guard's baseline version for a `memory_forget` (the generic
/// local-tool journal path already covers this via `local_snapshot::read_updated_at`
/// combined with `LocalTable::KmsFacts`; this is for `haily-kms`'s own
/// `KmsHandle::restore_fact` tests, which drive the restore directly, without going
/// through the journal).
pub async fn get_updated_at(db: &DbHandle, id: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT updated_at FROM kms_facts WHERE id = ?")
        .bind(id)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.map(|(v,)| v))
}

/// C10-guarded undo of `soft_delete`: clears `deleted_at` only if `updated_at` still
/// matches `expected_updated_at` — a record changed under us since the forward write
/// (e.g. a second forget, or any other concurrent touch) is detected by
/// `rows_affected()==0`, never a separate SELECT-then-UPDATE (TOCTOU). Returns `false`
/// on that race (caller must refuse, not retry blind); `true` once the DB half of the
/// restore has landed. The ANN-index half (`KmsHandle::restore_fact`) runs AFTER this
/// call returns `true` — see that function's doc for the resulting crash-ordering.
pub async fn clear_deleted_at(db: &DbHandle, id: &str, expected_updated_at: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE kms_facts SET deleted_at = NULL, updated_at = ?
         WHERE id = ? AND updated_at = ? AND deleted_at IS NOT NULL",
    )
    .bind(&now)
    .bind(id)
    .bind(expected_updated_at)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// Fact ids whose `deleted_at` was CLEARED (a `memory_forget` undo) strictly after
/// `since` (RFC3339), restricted to facts that existed BEFORE that boundary
/// (`created_at <= since`) — the C2 restore-vs-rebuild race fix.
///
/// `embeddings_created_since` cannot see these: an undo never changes `created_at`,
/// only `updated_at`. Without this delta, a `memory_forget` undone while a background
/// HNSW rebuild is in flight (`KmsHandle::index_remove`'s tombstone-ratio trigger)
/// would restore the OUTGOING (soon-to-be-discarded) index only — the fresh index's
/// own DB scan (`embeddings_for_hnsw`, captured before the restore's commit) already
/// excludes it, and the swap would silently drop the restored fact from ANN search.
/// Folding this delta in as an INSERT (mirroring `embeddings_created_since`'s shape)
/// at swap time closes that gap.
pub async fn ids_undeleted_since(db: &DbHandle, since: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let rows = sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT id, embedding FROM kms_facts
         WHERE embedding IS NOT NULL AND deleted_at IS NULL AND archived_at IS NULL
           AND updated_at > ? AND created_at <= ?",
    )
    .bind(since)
    .bind(since)
    .fetch_all(db.pool())
    .await?;
    Ok(rows)
}

/// EMA update: confidence = 0.8 * old + 0.2 * new_signal.
/// Archives the fact if confidence drops below 0.25.
pub async fn update_confidence(db: &DbHandle, id: &str, new_signal: f64) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE kms_facts
         SET confidence  = ROUND(0.8 * confidence + 0.2 * ?, 4),
             archived_at = CASE WHEN (0.8 * confidence + 0.2 * ?) < 0.25
                                THEN ? ELSE archived_at END,
             updated_at  = ?
         WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(new_signal)
    .bind(new_signal)
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}
