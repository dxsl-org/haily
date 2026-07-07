/// In-memory HNSW vector index with tombstone-based soft delete, threshold-triggered
/// rebuild, and dump/load persistence across restarts.
///
/// `hnsw_rs` 0.3.4 has no delete/update-in-place API (confirmed against crate source —
/// see phase-08 research report), so deletion is emulated with a tombstone set filtered
/// at query time, and periodically compacted away by a full rebuild from the DB (the
/// only source of truth for "which facts are live").
use chrono::{DateTime, Utc};
use hnsw_rs::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

const MAX_NB_CONNECTION: usize = 16;
const MAX_LAYER: usize = 16;
const EF_CONSTRUCTION: usize = 200;
const EF_SEARCH: usize = 64;
const INITIAL_CAPACITY: usize = 10_000;

/// Oversample factor applied to `k` before the tombstone/missing-id filter — absorbs
/// both tombstoned hits and (rare) dump/DB drift without starving the caller's `k`.
const OVERSAMPLE_FACTOR: usize = 3;

/// Rebuild trigger thresholds (research report §2): whichever fires first.
pub const REBUILD_TOMBSTONE_RATIO: f32 = 0.20;
pub const REBUILD_MAX_AGE_DAYS: i64 = 7;

const DUMP_BASENAME: &str = "kms";
const MANIFEST_FILENAME: &str = "kms_manifest.json";

/// Sits next to the `hnsw_rs` dump files. Ours to define and interpret — the crate
/// does not persist an id map or a "when was this built" timestamp for us.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DumpManifest {
    /// RFC3339 timestamp of the moment the dump was taken — the reconciliation
    /// anchor: facts created/deleted/archived after this must be replayed on load.
    pub dumped_at: String,
    /// Vector count at dump time. Verified against `Hnsw::get_nb_point()` after load
    /// — the torn-dump guard (red team): any mismatch triggers a full rebuild rather
    /// than silently serving an under-populated index.
    pub count: usize,
    /// Embedding model identifier — informational today (no cross-model migration
    /// path exists yet), kept so a future model swap can detect and reject a
    /// stale-model dump instead of silently loading incompatible vectors.
    pub model_id: String,
}

/// In-memory HNSW vector index.
/// Thread-safe: `Hnsw` uses interior mutability for parallel inserts; `id_map` and
/// `tombstones` use their own `RwLock`s. Callers needing atomic rebuild-and-swap
/// semantics hold this behind `RwLock<Arc<HnswIndex>>` (see `KmsHandle`) — this type
/// itself is just one immutable-after-construction snapshot of the graph plus its
/// live bookkeeping.
pub struct HnswIndex {
    hnsw: Hnsw<'static, f32, DistCosine>,
    /// Maps HNSW numeric id → UUID string of the corresponding fact.
    id_map: RwLock<Vec<String>>,
    /// Fact ids removed since this index was built (soft-delete/archive/forget).
    /// Filtered out at query time; cleared implicitly by the next rebuild (a rebuild
    /// reads straight from the DB, which already excludes these facts).
    tombstones: RwLock<HashSet<String>>,
    last_rebuild: RwLock<DateTime<Utc>>,
}

impl HnswIndex {
    pub fn new() -> Self {
        Self {
            hnsw: Hnsw::<'static, f32, DistCosine>::new(
                MAX_NB_CONNECTION,
                INITIAL_CAPACITY,
                MAX_LAYER,
                EF_CONSTRUCTION,
                DistCosine,
            ),
            id_map: RwLock::new(Vec::new()),
            tombstones: RwLock::new(HashSet::new()),
            last_rebuild: RwLock::new(Utc::now()),
        }
    }

    /// Batch-insert all facts at startup. Uses rayon parallelism internally.
    /// `items`: Vec<(uuid_string, embedding_f32_vec)>
    pub fn batch_insert(&self, items: &[(String, Vec<f32>)]) {
        if items.is_empty() {
            return;
        }
        let start_idx = {
            let mut map = self.id_map.write().expect("id_map write lock");
            let start = map.len();
            for (id, _) in items {
                map.push(id.clone());
            }
            start
        };
        let slices: Vec<(&[f32], usize)> = items
            .iter()
            .enumerate()
            .map(|(i, (_, emb))| (emb.as_slice(), start_idx + i))
            .collect();
        self.hnsw.parallel_insert_slice(&slices);
    }

    /// Insert a single new fact after it has been stored in the DB.
    pub fn insert(&self, id: &str, embedding: &[f32]) {
        let idx = {
            let mut map = self.id_map.write().expect("id_map write lock");
            let idx = map.len();
            map.push(id.to_string());
            idx
        };
        let slice: Vec<(&[f32], usize)> = vec![(embedding, idx)];
        self.hnsw.parallel_insert_slice(&slice);
    }

    /// Mark a fact id as removed. No-op if the id was never indexed (e.g. a fact
    /// stored without an embedding) — tombstoning an absent id is harmless.
    pub fn tombstone(&self, id: &str) {
        self.tombstones.write().expect("tombstones write lock").insert(id.to_string());
    }

    pub fn is_tombstoned(&self, id: &str) -> bool {
        self.tombstones.read().expect("tombstones read lock").contains(id)
    }

    pub fn tombstone_count(&self) -> usize {
        self.tombstones.read().expect("tombstones read lock").len()
    }

    /// Fraction of the index's indexed vectors that are tombstoned. `0.0` on an empty
    /// index (avoids a div-by-zero false trigger on a fresh/empty store).
    pub fn tombstone_ratio(&self) -> f32 {
        let total = self.len();
        if total == 0 {
            return 0.0;
        }
        self.tombstone_count() as f32 / total as f32
    }

    pub fn last_rebuild(&self) -> DateTime<Utc> {
        *self.last_rebuild.read().expect("last_rebuild read lock")
    }

    /// Whether a compaction rebuild should run now: tombstone ratio past the
    /// watermark, or the index has gone stale by wall-clock age — whichever fires
    /// first (research report §2's hybrid trigger).
    pub fn should_rebuild(&self) -> bool {
        if self.tombstone_ratio() > REBUILD_TOMBSTONE_RATIO {
            return true;
        }
        Utc::now() - self.last_rebuild() > chrono::Duration::days(REBUILD_MAX_AGE_DAYS)
    }

    /// ANN search. Returns up to `k` `(uuid, distance)` pairs ordered by distance
    /// ascending, with tombstoned ids filtered out.
    ///
    /// Oversamples `k * OVERSAMPLE_FACTOR` from the underlying graph before filtering
    /// — a plain `k` request would silently under-return once tombstones exist,
    /// since the graph itself still has no concept of deletion.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        if k == 0 {
            return Vec::new();
        }
        let raw_k = k.saturating_mul(OVERSAMPLE_FACTOR).max(k);
        let neighbours = self.hnsw.search(query, raw_k, EF_SEARCH);
        let map = self.id_map.read().expect("id_map read lock");
        let tombstones = self.tombstones.read().expect("tombstones read lock");
        neighbours
            .into_iter()
            .filter_map(|n| map.get(n.d_id).map(|id| (id.clone(), n.distance)))
            .filter(|(id, _)| !tombstones.contains(id))
            .take(k)
            .collect()
    }

    /// Total vectors ever inserted into this index instance (tombstoned or not) —
    /// NOT "live count"; a rebuild is the only way to shrink this.
    pub fn len(&self) -> usize {
        self.id_map.read().expect("id_map read lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Build a fresh index from `items`, independent of `self` — used by the
    /// atomic-swap rebuild path (`KmsHandle::rebuild_index`), which constructs this
    /// off to the side and only swaps the shared pointer once it is fully populated,
    /// so in-flight searches against the OLD index never observe a partially built
    /// graph.
    pub fn build_from(items: &[(String, Vec<f32>)]) -> Self {
        let index = Self::new();
        index.batch_insert(items);
        *index.last_rebuild.write().expect("last_rebuild write lock") = Utc::now();
        index
    }

    /// Dump the graph + a sidecar id-map + manifest to `dir`.
    ///
    /// **Torn-dump guard:** `hnsw_rs::file_dump` (and our own sidecar writes) go to a
    /// temp basename first; only after every file is written and flushed do we
    /// rename each into its final name. A rename within the same directory is a
    /// single filesystem metadata operation on both Windows and Unix — a process
    /// killed mid-dump leaves the temp files orphaned (harmless, next dump overwrites
    /// them) but never leaves a half-written file under the name `load()` will read.
    pub fn dump(&self, dir: &Path, model_id: &str) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp_basename = format!("{DUMP_BASENAME}.tmp-{}", uuid::Uuid::new_v4());

        // `file_dump` (via the `AnnT` trait) takes `&self` — this can run while
        // searches are still in flight against this same (about-to-be-retired) index.
        let dumped_basename = self
            .hnsw
            .file_dump(dir, &tmp_basename)
            .map_err(|e| anyhow::anyhow!("hnsw file_dump failed: {e:#}"))?;

        let id_map = self.id_map.read().expect("id_map read lock").clone();
        let tmp_id_map_path = dir.join(format!("{tmp_basename}.id_map.json"));
        std::fs::write(&tmp_id_map_path, serde_json::to_vec(&id_map)?)?;

        let manifest = DumpManifest {
            dumped_at: Utc::now().to_rfc3339(),
            count: id_map.len(),
            model_id: model_id.to_string(),
        };
        let tmp_manifest_path = dir.join(format!("{tmp_basename}.manifest.json"));
        std::fs::write(&tmp_manifest_path, serde_json::to_vec(&manifest)?)?;

        // Rename graph/data/id_map/manifest into their final names — last, so a crash
        // between any of these renames still leaves `load()` seeing either the fully
        // old set (final names untouched) or triggers a load failure on a partial
        // final set, both of which fall back to rebuild rather than corrupting state.
        std::fs::rename(
            dir.join(format!("{dumped_basename}.hnsw.graph")),
            dir.join(format!("{DUMP_BASENAME}.hnsw.graph")),
        )?;
        std::fs::rename(
            dir.join(format!("{dumped_basename}.hnsw.data")),
            dir.join(format!("{DUMP_BASENAME}.hnsw.data")),
        )?;
        std::fs::rename(&tmp_id_map_path, dir.join(format!("{DUMP_BASENAME}.id_map.json")))?;
        std::fs::rename(&tmp_manifest_path, dir.join(MANIFEST_FILENAME))?;

        Ok(())
    }

    /// Load a previously dumped index from `dir`. Returns `Ok(None)` when no dump
    /// exists yet (first run — not an error). Returns `Err` for any other failure
    /// (missing sidecar, corrupt JSON, count mismatch, crate-level load error) — the
    /// caller (`KmsHandle::init`) treats every `Err` the same way: log and fall back
    /// to a full rebuild from the DB, per the "load is purely an optimization, never
    /// a new failure mode" design (research report §4).
    pub fn load(dir: &Path) -> anyhow::Result<Option<(Self, DumpManifest)>> {
        let manifest_path = dir.join(MANIFEST_FILENAME);
        if !manifest_path.exists() {
            return Ok(None);
        }
        let manifest: DumpManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path)?)?;

        let id_map_path = dir.join(format!("{DUMP_BASENAME}.id_map.json"));
        let id_map: Vec<String> = serde_json::from_slice(&std::fs::read(&id_map_path)?)?;

        // `HnswIo::load_hnsw`'s signature ties the returned `Hnsw<'b,...>` to the
        // loader via `'a: 'b`, even though with `ReloadOptions::default()` (no mmap)
        // the returned value never actually reads through that reference again after
        // this call returns (verified against crate source: mmap is the only path
        // that keeps a live pointer into the loader's data). `Box::leak` turns the
        // bound into a real `'static` the type-checker accepts — a one-time, bounded
        // (one small struct per load/rebuild, not per-query) leak, not unbounded
        // growth.
        let reloader: &'static mut HnswIo = Box::leak(Box::new(HnswIo::new(dir, DUMP_BASENAME)));
        let hnsw: Hnsw<'static, f32, DistCosine> = reloader
            .load_hnsw::<f32, DistCosine>()
            .map_err(|e| anyhow::anyhow!("hnsw load_hnsw failed: {e:#}"))?;

        let loaded_count = hnsw.get_nb_point();
        // Torn-dump guard: the manifest's own recorded count, the id_map length, AND
        // the crate's own post-load point count must all agree. A truncated dump can
        // pass the crate's internal magic-number check yet still under-populate the
        // graph — this three-way check is what catches that case (red team).
        if loaded_count != manifest.count || id_map.len() != manifest.count {
            return Err(anyhow::anyhow!(
                "hnsw dump count mismatch: manifest={}, id_map={}, loaded={loaded_count}",
                manifest.count,
                id_map.len(),
            ));
        }

        let index = Self {
            hnsw,
            id_map: RwLock::new(id_map),
            tombstones: RwLock::new(HashSet::new()),
            last_rebuild: RwLock::new(
                DateTime::parse_from_rfc3339(&manifest.dumped_at)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            ),
        };
        Ok(Some((index, manifest)))
    }

    /// Apply a facts delta after a successful load: insert facts created since the
    /// dump, and tombstone facts deleted/archived since the dump. Order matters only
    /// in that a fact appearing in both lists (created and later deleted, both after
    /// the dump) must end up tombstoned — inserting first then tombstoning achieves
    /// that regardless of which list it came from.
    pub fn apply_delta(
        &self,
        newly_created: &[(String, Vec<f32>)],
        newly_removed: &[String],
    ) {
        // Dedup against ids already indexed: a background rebuild captures its delta
        // boundary BEFORE its DB read, so a fact created in that overlap window is in
        // both the fresh build AND `newly_created`; inserting it twice would map two
        // graph points to one fact id. Skipping already-present ids keeps the delta
        // idempotent for both the startup-load and background-rebuild callers.
        let existing: std::collections::HashSet<String> =
            self.id_map.read().expect("id_map read lock").iter().cloned().collect();
        let fresh: Vec<(String, Vec<f32>)> = newly_created
            .iter()
            .filter(|(id, _)| !existing.contains(id))
            .cloned()
            .collect();
        self.batch_insert(&fresh);
        for id in newly_removed {
            self.tombstone(id);
        }
    }

    /// Snapshot of every currently-tombstoned id. Used at background-rebuild swap time
    /// to carry tombstones applied to the outgoing index into the fresh one, so a fact
    /// forgotten during the rebuild window cannot reappear after the pointer swap.
    pub fn tombstoned_ids(&self) -> Vec<String> {
        self.tombstones.read().expect("tombstones read lock").iter().cloned().collect()
    }

    /// True if `id` has EVER been inserted into `id_map` — regardless of tombstone
    /// status. This is the branch a `memory_forget` undo (`KmsHandle::restore_fact`)
    /// uses to decide `un_tombstone` (id still physically present — no compaction has
    /// run since it was forgotten) vs. re-insert-from-BLOB (a rebuild already dropped
    /// it from the graph). Linear scan over `id_map`: acceptable here because a
    /// restore is a rare, user-initiated undo action, not a hot path like `search`.
    pub fn contains(&self, id: &str) -> bool {
        self.id_map.read().expect("id_map read lock").iter().any(|existing| existing == id)
    }

    /// Clear a tombstone (undo of `tombstone`), restoring ANN visibility for an id that
    /// is STILL present in `id_map`. Callers MUST check `contains(id)` first — calling
    /// this for an id absent from `id_map` is a silent no-op on the tombstone set (the
    /// id was never hiding anything), never a `contains`-vs-`insert` substitute: an
    /// absent id must go through `insert` (from the stored embedding BLOB) instead, or
    /// it stays permanently unsearchable.
    pub fn un_tombstone(&self, id: &str) {
        self.tombstones.write().expect("tombstones write lock").remove(id);
    }
}

impl Default for HnswIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Absolute path to the directory holding the HNSW dump files, derived from the
/// app's data directory (kept alongside `haily.db` so a copied install directory
/// carries the index cache with it, matching `haily-app::default_data_dir`'s
/// portable-first convention).
pub fn dump_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("hnsw")
}
