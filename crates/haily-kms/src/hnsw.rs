use hnsw_rs::prelude::*;
use std::sync::RwLock;

const MAX_NB_CONNECTION: usize = 16;
const MAX_LAYER: usize = 16;
const EF_CONSTRUCTION: usize = 200;
const EF_SEARCH: usize = 64;
const INITIAL_CAPACITY: usize = 10_000;

/// In-memory HNSW vector index.
/// Thread-safe: Hnsw uses interior mutability for parallel inserts; id_map uses RwLock.
/// Rebuilt from DB embeddings at startup; updated in-place when new facts are inserted.
pub struct HnswIndex {
    hnsw: Hnsw<'static, f32, DistCosine>,
    /// Maps HNSW numeric id → UUID string of the corresponding fact.
    id_map: RwLock<Vec<String>>,
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

    /// ANN search. Returns `(uuid, distance)` pairs ordered by distance ascending.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        let neighbours = self.hnsw.search(query, k, EF_SEARCH);
        let map = self.id_map.read().expect("id_map read lock");
        neighbours
            .into_iter()
            .filter_map(|n| map.get(n.d_id).map(|id| (id.clone(), n.distance)))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.id_map.read().expect("id_map read lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for HnswIndex {
    fn default() -> Self {
        Self::new()
    }
}
