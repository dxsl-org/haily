pub mod authored_skills;
pub mod distillation;
pub mod feedback;
pub mod hnsw;
pub mod kit_pack;
pub mod search;
pub mod skills;
pub mod system_prompt;
pub mod voice_check;

#[cfg(feature = "embeddings")]
pub mod embedder;

use anyhow::Result;
use haily_db::{queries::facts, queries::meta, queries::skills as db_skills, DbHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use uuid::Uuid;

#[cfg(feature = "embeddings")]
use embedder::Embedder;

use hnsw::HnswIndex;

/// Embedding model identifier recorded in the HNSW dump manifest. Bumping this string
/// (e.g. on a model upgrade) makes an old dump's `model_id` mismatch detectable —
/// today it is informational only (no cross-model reconciliation path exists), but
/// a future check can key off it to force a rebuild instead of loading vectors from
/// a different embedding space.
const EMBEDDING_MODEL_ID: &str = "multilingual-e5-base";

pub struct KmsHandle {
    pub(crate) db: DbHandle,
    /// `RwLock<Arc<..>>` (not a bare `Arc`) so a background rebuild can construct a
    /// fresh index off to the side and swap the pointer atomically — in-flight
    /// searches hold their own `Arc` clone taken before the swap and are unaffected.
    hnsw: RwLock<Arc<HnswIndex>>,
    /// Directory holding the HNSW dump files (`hnsw::dump_dir(data_dir)`).
    dump_dir: PathBuf,
    /// The app data directory (phase 8). The distillation standards-overlay lives HERE, OUTSIDE
    /// any coding-workspace `fs_write` root (SEC-H) — so an auto-approved `fs_write` can never
    /// self-persist a prompt injection into the overlay.
    data_dir: PathBuf,
    /// Prevents two rebuilds (e.g. a tombstone-ratio trigger firing twice in quick
    /// succession from concurrent `index_remove` calls) from running concurrently.
    rebuild_in_progress: AtomicBool,
    /// In-memory authored-skill registry loaded from the kit-pack (phase-02). Entirely
    /// separate from the synthesized `kms_skills` lifecycle — file-sourced, read-only.
    authored: authored_skills::AuthoredRegistry,
    #[cfg(feature = "embeddings")]
    embedder: Arc<Embedder>,
}

impl KmsHandle {
    /// Initialise KMS: load the HNSW index from its on-disk dump under
    /// `data_dir` (fast path) or rebuild it from persisted embeddings (fallback —
    /// also the path taken on first run, before any dump exists). With `embeddings`
    /// feature: also init the fastembed model (downloads ~150 MB on first run).
    ///
    /// Load failure (missing dump, corrupt/torn files, count mismatch, or a version
    /// mismatch in `hnsw_rs`'s own dump format) is caught and logged, then falls back
    /// to the full rebuild — this makes the dump a pure optimization with no new
    /// failure mode, per phase-08's design.
    pub async fn init(db: DbHandle, data_dir: &Path) -> Result<Self> {
        let dump_dir = hnsw::dump_dir(data_dir);
        let hnsw = Self::load_or_rebuild(&db, &dump_dir).await?;

        // Phase-02: load the authored-skill kit-pack (tolerant of absence — a missing or
        // unverifiable pack logs and yields an empty registry, never fails boot).
        let authored = Self::load_authored(data_dir);

        #[cfg(feature = "embeddings")]
        let embedder = {
            let emb = tokio::task::spawn_blocking(Embedder::init).await??;
            Arc::new(emb)
        };

        Ok(Self {
            db,
            hnsw: RwLock::new(Arc::new(hnsw)),
            dump_dir,
            data_dir: data_dir.to_path_buf(),
            rebuild_in_progress: AtomicBool::new(false),
            authored,
            #[cfg(feature = "embeddings")]
            embedder,
        })
    }

    /// Locate and load the kit-pack into an [`authored_skills::AuthoredRegistry`].
    ///
    /// Source precedence: `<data_dir>/kit-pack` (the shipped/packaged location) first,
    /// else a CWD-relative `assets/kit-pack` (dev/`cargo run` from the repo root). Only
    /// the kit-pack tier is populated today; the merge supports the full 5-tier order.
    /// Any absence or load failure yields an empty registry (logged, never fatal).
    fn load_authored(data_dir: &Path) -> authored_skills::AuthoredRegistry {
        let Some(dir) = Self::kit_pack_source(data_dir) else {
            tracing::debug!("no kit-pack found — authored-skill registry is empty");
            return authored_skills::AuthoredRegistry::new();
        };
        match kit_pack::load(&dir) {
            Ok(skills) => {
                tracing::info!(count = skills.len(), path = %dir.display(), "kit-pack loaded");
                // kit-pack is the LOWEST precedence tier (user/project tiers, when they
                // exist, override it by name).
                authored_skills::AuthoredRegistry::from_tiers(vec![skills])
            }
            Err(e) => {
                tracing::warn!(path = %dir.display(), "kit-pack load failed — continuing without authored skills: {e:#}");
                authored_skills::AuthoredRegistry::new()
            }
        }
    }

    /// First existing kit-pack directory (one that contains a `manifest.json`).
    fn kit_pack_source(data_dir: &Path) -> Option<PathBuf> {
        let packaged = data_dir.join("kit-pack");
        if packaged.join("manifest.json").is_file() {
            return Some(packaged);
        }
        let dev = PathBuf::from("assets/kit-pack");
        if dev.join("manifest.json").is_file() {
            return Some(dev);
        }
        None
    }

    // ---------------------------------------------------------------------
    // Authored-skill API (phase-02) — thin wrappers over `self.authored`.
    // ---------------------------------------------------------------------

    /// Compact L0 routing table (name + when_to_use), for the `## Skills` system-prompt
    /// section. Empty string when no kit-pack is loaded.
    pub fn authored_routing_table(&self) -> String {
        self.authored.routing_table()
    }

    /// Top-`k` authored playbook `(name, body)` pairs relevant to `task`, domain-filtered.
    /// References stay unloaded (progressive disclosure).
    pub fn authored_playbooks_for(
        &self,
        task: &str,
        domain: Option<&str>,
        k: usize,
    ) -> Vec<(String, String)> {
        self.authored.playbooks_for(task, domain, k)
    }

    /// Standard-kind bodies for the named standards (e.g. `["lang-rust"]`), for the
    /// sub-turn `## Standards` injection after stack detection.
    pub fn authored_standards_for(&self, names: &[&str]) -> Vec<(String, String)> {
        self.authored.standards_for(names)
    }

    /// Path to the distillation standards-overlay file (phase 8). Deliberately under the app
    /// DATA dir, never a coding-workspace worktree — an auto-approved `fs_write` cannot reach
    /// it (SEC-H). Its presence is not required; readers fail-open on absence.
    pub fn standards_overlay_path(&self) -> PathBuf {
        self.data_dir.join("standards-overlay.md")
    }

    /// Non-expired, approval-provenanced overlay standards as `(heading, body)` pairs, for the
    /// sub-turn `## Standards` injection AFTER the kit standards (phase 8). Fail-open (empty on
    /// any read problem / no overlay yet).
    pub fn overlay_standards(&self) -> Vec<(String, String)> {
        let now = chrono::Utc::now().to_rfc3339();
        distillation::load_overlay_standards(&self.standards_overlay_path(), &now)
    }

    /// Synthesized skills (confidence ≥ [`skills::SYNTH_SKILL_MIN_CONFIDENCE`]) whose pattern
    /// matches `task`, as top-`top_n` `(heading, body)` playbook pairs for the sub-turn
    /// `## Playbooks` pool (phase 8). Source is made visible in each heading. A DB read failure
    /// yields an empty pool (never an error) — a learning signal must never break a turn.
    pub async fn synthesized_playbooks_for(&self, task: &str, top_n: usize) -> Vec<(String, String)> {
        match db_skills::active_skills(&self.db).await {
            Ok(active) => skills::synthesized_playbooks(
                &active,
                task,
                skills::SYNTH_SKILL_MIN_CONFIDENCE,
                top_n,
            ),
            Err(e) => {
                tracing::warn!("synthesized_playbooks_for: active_skills read failed: {e:#}");
                Vec::new()
            }
        }
    }

    /// Enumerate a skill's fetchable sections (`body` + reference chunk ids). Errors on
    /// an unknown skill.
    pub fn list_skill_sections(&self, skill: &str) -> Result<Vec<(String, String)>> {
        self.authored.list_sections(skill)
    }

    /// Fetch exactly ONE section of a skill (the runtime-mediated lazy-load). Errors on
    /// an unknown skill or section — never dumps the whole skill.
    pub fn fetch_skill_section(&self, skill: &str, section: &str) -> Result<String> {
        self.authored.fetch_section(skill, section)
    }

    /// Discovery: authored skills relevant to `query` as `(name, when_to_use)`.
    pub fn search_skills(&self, query: &str, k: usize) -> Vec<(String, String)> {
        self.authored.search(query, k)
    }

    /// Attempt to load a valid dump; on any failure (including "no dump yet"), fall
    /// back to a full rebuild from `facts::embeddings_for_hnsw`. On a successful
    /// load, reconciles the delta between the dump timestamp and now (facts created
    /// since are inserted; facts deleted/archived since are tombstoned) so a clean
    /// restart never serves stale-but-live or resurrected-dead results.
    async fn load_or_rebuild(db: &DbHandle, dump_dir: &Path) -> Result<HnswIndex> {
        match HnswIndex::load(dump_dir) {
            Ok(Some((index, manifest))) => {
                tracing::info!(count = index.len(), dumped_at = %manifest.dumped_at, "hnsw loaded");

                let created = facts::embeddings_created_since(db, &manifest.dumped_at)
                    .await
                    .unwrap_or_default();
                // C2: a `memory_forget` undone between the dump and this load has an
                // unchanged `created_at`, so `embeddings_created_since` alone would miss
                // it — see `facts::ids_undeleted_since`'s doc.
                let undeleted = facts::ids_undeleted_since(db, &manifest.dumped_at)
                    .await
                    .unwrap_or_default();
                let removed = facts::ids_deleted_or_archived_since(db, &manifest.dumped_at)
                    .await
                    .unwrap_or_default();
                if !created.is_empty() || !undeleted.is_empty() || !removed.is_empty() {
                    let created_floats: Vec<(String, Vec<f32>)> = created
                        .into_iter()
                        .chain(undeleted)
                        .map(|(id, blob)| (id, blob_to_floats(&blob)))
                        .collect();
                    tracing::info!(
                        inserted = created_floats.len(),
                        tombstoned = removed.len(),
                        "hnsw delta reconciliation"
                    );
                    index.apply_delta(&created_floats, &removed);
                }
                Ok(index)
            }
            Ok(None) => {
                tracing::info!("no hnsw dump found — building from DB");
                Self::rebuild_from_db(db).await
            }
            Err(e) => {
                tracing::warn!("hnsw dump load failed, rebuilding from DB: {e:#}");
                Self::rebuild_from_db(db).await
            }
        }
    }

    /// Full rebuild from `facts::embeddings_for_hnsw` — the DB is always the source
    /// of truth, so this is naturally tombstone-free (the query already excludes
    /// soft-deleted/archived rows).
    async fn rebuild_from_db(db: &DbHandle) -> Result<HnswIndex> {
        let rows = facts::embeddings_for_hnsw(db).await?;
        let count = rows.len();
        let index = tokio::task::spawn_blocking(move || {
            let items: Vec<(String, Vec<f32>)> = rows
                .into_iter()
                .map(|(id, blob)| (id, blob_to_floats(&blob)))
                .collect();
            HnswIndex::build_from(&items)
        })
        .await?;
        tracing::info!(count, "hnsw rebuilt from DB");
        Ok(index)
    }

    /// Snapshot `Arc` clone of the current index — callers hold this for the
    /// duration of one operation (search/insert) so a concurrent rebuild-and-swap
    /// never invalidates a search already in flight.
    fn hnsw_snapshot(&self) -> Arc<HnswIndex> {
        Arc::clone(&self.hnsw.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Tombstone a fact id in the live index — called after `facts::soft_delete`,
    /// `facts::archive`, or `memory_forget` removes it from the DB, so it stops
    /// surfacing from ANN search in THIS process immediately (no restart needed).
    ///
    /// Opportunistically kicks off a background compaction rebuild when the
    /// tombstone ratio (or 7-day age) threshold is crossed. Takes `self: &Arc<Self>`
    /// (rather than plain `&self`) so the background task can hold its own `Arc`
    /// clone and run fully detached — every real caller already reaches `KmsHandle`
    /// through an `Arc` (`ToolContext.kms`, `Orchestrator.kms`), so this is not a
    /// wider constraint than the codebase already has.
    pub fn index_remove(self: &Arc<Self>, id: &str) {
        let snapshot = self.hnsw_snapshot();
        snapshot.tombstone(id);

        if snapshot.should_rebuild()
            && self
                .rebuild_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            let this = Arc::clone(self);
            let old = snapshot; // outgoing index — carries tombstones added so far
            tracing::info!("hnsw tombstone/age threshold crossed — starting background rebuild");
            tokio::spawn(async move {
                // Capture the reconciliation boundary BEFORE `rebuild_from_db` reads the
                // DB. A `remember`/`forget` committing while the rebuild runs would
                // otherwise mutate only the outgoing index and be lost at the swap; the
                // deltas below fold those mutations into the fresh index instead.
                let since = chrono::Utc::now().to_rfc3339();
                match Self::rebuild_from_db(&this.db).await {
                    Ok(fresh) => {
                        let created = facts::embeddings_created_since(&this.db, &since)
                            .await
                            .unwrap_or_default();
                        // C2: fold in any `memory_forget` undone during this rebuild's
                        // window — its `created_at` predates `since`, so it would
                        // otherwise be silently dropped at the swap below. See
                        // `facts::ids_undeleted_since`'s doc for the full race.
                        let undeleted = facts::ids_undeleted_since(&this.db, &since)
                            .await
                            .unwrap_or_default();
                        let removed = facts::ids_deleted_or_archived_since(&this.db, &since)
                            .await
                            .unwrap_or_default();
                        let created_floats: Vec<(String, Vec<f32>)> = created
                            .into_iter()
                            .chain(undeleted)
                            .map(|(id, blob)| (id, blob_to_floats(&blob)))
                            .collect();
                        fresh.apply_delta(&created_floats, &removed);
                        // Union tombstones the outgoing index accumulated during the
                        // window — covers a forget whose DB delete raced the `since`
                        // boundary, so a fact forgotten mid-rebuild stays gone.
                        fresh.apply_delta(&[], &old.tombstoned_ids());

                        let mut guard = this.hnsw.write().unwrap_or_else(|e| e.into_inner());
                        *guard = Arc::new(fresh);
                        tracing::info!(
                            reconciled_inserts = created_floats.len(),
                            "hnsw background rebuild complete — index swapped"
                        );
                    }
                    Err(e) => tracing::warn!("hnsw background rebuild failed: {e:#}"),
                }
                this.rebuild_in_progress.store(false, Ordering::SeqCst);
            });
        }
    }

    /// Dump the current index to disk. Called from the app's graceful-shutdown flush
    /// hook (`AppHandle::shutdown`). Dump failure is logged, not propagated: losing
    /// the on-disk cache only costs the next startup a full rebuild, never data
    /// (SQLite remains the source of truth), so it must never block or fail shutdown.
    pub async fn flush_index(&self) {
        let dir = self.dump_dir.clone();
        let snapshot = self.hnsw_snapshot();
        let result = tokio::task::spawn_blocking(move || snapshot.dump(&dir, EMBEDDING_MODEL_ID)).await;
        match result {
            Ok(Ok(())) => tracing::info!("hnsw index dumped for next startup"),
            Ok(Err(e)) => tracing::warn!("hnsw dump failed (next start will rebuild): {e:#}"),
            Err(e) => tracing::warn!("hnsw dump task panicked (next start will rebuild): {e:#}"),
        }
    }

    /// Direct ANN search against a caller-supplied vector, bypassing the embedder
    /// entirely — the underlying HNSW path works identically with or without the
    /// `embeddings` feature (it stores/searches whatever `f32` vectors it is given),
    /// so this stays available in every build rather than being feature-gated.
    /// Returns `(fact_id, cosine_distance)` pairs.
    pub async fn search_ann_by_vector(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        self.hnsw_snapshot().search(query, k)
    }

    /// Whether `id` is currently ANN-*indexed and live*: present in the in-memory graph's
    /// id map AND not tombstoned. This is the exact, deterministic contract `restore_fact`
    /// and `remember` fulfil (graph membership + tombstone state) and the precise inverse of
    /// what `index_remove` establishes.
    ///
    /// It deliberately does NOT run an approximate ANN query: HNSW recall is not a hard
    /// guarantee — a greedy layered search over a small or tightly-clustered graph can return
    /// fewer than `k` neighbours and miss even an exact (distance-0) match, and the graph
    /// topology itself varies run-to-run because `parallel_insert_slice` builds it under rayon.
    /// Callers needing end-to-end recall of a restored fact use `search_hybrid`, whose FTS5
    /// leg is exact; this method answers the narrower, fully-deterministic question "did the
    /// index re-admit this id and clear its tombstone".
    pub fn is_ann_indexed(&self, id: &str) -> bool {
        let snapshot = self.hnsw_snapshot();
        snapshot.contains(id) && !snapshot.is_tombstoned(id)
    }

    /// Hybrid search: FTS5 BM25 always; HNSW ANN when embeddings feature is active.
    /// Returns a ranked list of fact texts relevant to `query`.
    pub async fn search_hybrid(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<search::SearchResult>> {
        let snapshot = self.hnsw_snapshot();
        #[cfg(feature = "embeddings")]
        {
            let embedder = Arc::clone(&self.embedder);
            let query_owned = query.to_string();
            let qv = tokio::task::spawn_blocking(move || embedder.embed_query(&query_owned))
                .await??;
            search::hybrid(&self.db, &snapshot, Some(&qv), query, limit).await
        }
        #[cfg(not(feature = "embeddings"))]
        {
            search::hybrid(&self.db, &snapshot, None, query, limit).await
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

            self.hnsw_snapshot().insert(&fact.id, &embedding);
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

    /// Soft-delete a fact and remove it from ANN search immediately (same process,
    /// no restart) — the tombstone-then-DB ordering here is deliberate: if the
    /// process is killed between the two, the DB row (source of truth) is not yet
    /// deleted and a later ANN hit is merely a false-positive candidate that
    /// `search::hybrid`'s DB refetch (for ANN-only hits) or `search_fts`'s
    /// `deleted_at IS NULL` filter (for FTS hits) will still filter out on the read
    /// path — reversing the order (DB first) risks a crash leaving the fact deleted
    /// in SQLite but still ANN-reachable until the next rebuild, which is worse.
    pub async fn forget_fact(self: &Arc<Self>, id: &str) -> Result<bool> {
        let removed = facts::soft_delete(&self.db, id).await?;
        if removed {
            self.index_remove(id);
        }
        Ok(removed)
    }

    /// Undo a `memory_forget`: clear `deleted_at` (DB, the source of truth — runs
    /// FIRST) then restore ANN searchability in the live in-memory index.
    ///
    /// C10: guarded by `expected_updated_at` exactly like the local-tool compensator's
    /// `clear_deleted_at` — a record changed under us since the forward write (a
    /// second forget, or any other concurrent touch) affects zero DB rows; this
    /// returns `Ok(false)` and the caller must treat it as a refusal, never retry
    /// blind. `Ok(true)` once the DB half has landed (the ANN half below cannot fail
    /// in a way that should un-do the DB clear — a fact merely missing from ANN
    /// still exists and is findable via FTS/DB lookups).
    ///
    /// Crash-ordering: if the process dies between the DB clear (committed) and the
    /// ANN restore (not yet run), the fact is LIVE in SQLite but still
    /// tombstoned/absent from the in-memory HNSW graph — benign and self-healing:
    /// `search::hybrid`'s FTS5 leg and any DB-backed lookup already find it; ANN
    /// search alone misses it until the next tombstone/age-triggered rebuild
    /// (`index_remove`) or a process restart (`load_or_rebuild`/`rebuild_from_db`,
    /// both DB-driven). The reverse ordering (ANN first) would risk the opposite and
    /// strictly worse failure — an ANN hit for a fact SQLite still shows deleted —
    /// which every read path would then have to defensively re-filter.
    ///
    /// Branches on `HnswIndex::contains` per the un-tombstone-vs-reinsert contract: a
    /// bare `insert` on a still-present id would `push` a SECOND `id_map` entry for
    /// the same fact id (no dedup in `HnswIndex::insert`), corrupting the id→index
    /// mapping. `contains` false means a compaction rebuild already dropped the node
    /// from the graph — re-insert from the still-present `kms_facts.embedding` BLOB
    /// (soft-delete never clears it — see `facts::soft_delete`), never re-embedded.
    ///
    /// Defensive `un_tombstone` after a re-insert: a background rebuild
    /// (`index_remove`) unions the OUTGOING index's tombstones into the fresh one
    /// (`old.tombstoned_ids()`) so a fact forgotten mid-rebuild stays gone — but that
    /// same union can carry a STALE tombstone for THIS id forward if this restore's
    /// `un_tombstone` (the `contains` branch, on a DIFFERENT prior index instance)
    /// raced the carry-forward read. Clearing it again here, unconditionally, after a
    /// fresh insert is a no-op when no stale entry exists and closes that gap when one
    /// does — a just-restored fact must never end up hidden by a leftover tombstone.
    pub async fn restore_fact(&self, id: &str, expected_updated_at: &str) -> Result<bool> {
        if !facts::clear_deleted_at(&self.db, id, expected_updated_at).await? {
            return Ok(false);
        }
        let snapshot = self.hnsw_snapshot();
        if snapshot.contains(id) {
            snapshot.un_tombstone(id);
        } else if let Some(fact) = facts::get_fact(&self.db, id).await? {
            if let Some(blob) = fact.embedding {
                snapshot.insert(id, &blob_to_floats(&blob));
                snapshot.un_tombstone(id);
            }
        }
        Ok(true)
    }

    /// Archive a fact (distinct from soft-delete — see `facts::archive`) and remove
    /// it from ANN search immediately. Same ordering rationale as `forget_fact`.
    pub async fn archive_fact(self: &Arc<Self>, id: &str) -> Result<bool> {
        let archived = facts::archive(&self.db, id).await?;
        if archived {
            self.index_remove(id);
        }
        Ok(archived)
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
            // Phase-02: the authored-skill index (progressive-disclosure level 1) that
            // the `## Skills` system-prompt section renders. Empty when no kit-pack.
            skill_routing_table: self.authored_routing_table(),
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
    /// Phase-02: compact authored-skill routing table (name + when_to_use, one line
    /// each) rendered as the L0 `## Skills` section. Empty when no kit-pack is loaded.
    pub skill_routing_table: String,
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

/// Decode a little-endian `f32` blob (SQLite BLOB storage format for embeddings —
/// see `Embedder::to_bytes`/`from_bytes`) without requiring the `embeddings` feature,
/// since HNSW rebuild/reconcile paths need to read stored vectors regardless of
/// whether this build can generate new ones.
fn blob_to_floats(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
