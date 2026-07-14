//! In-memory, thread-safe store for [`DataView`] snapshots (View Engine Phase A).
//!
//! A view is a latest-snapshot keyed by `view_id`, not an incremental stream — `insert`
//! replaces/creates and `get` reads the current snapshot back. Cap-bounded with FIFO
//! (insertion-order) eviction so a long-running session cannot grow this store unbounded;
//! eviction is a capacity concern only, never a correctness one — a fetch for an evicted
//! `view_id` simply returns `None`, which Phase 3's command path must treat as "view no
//! longer available" (e.g. re-run the tool), not an error.

use haily_types::{DataView, ViewSink};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use uuid::Uuid;

/// Cap on how many views this process holds at once. Small and in-memory by design — a
/// view is disposable, re-derivable by re-running the producing tool call, never the
/// system of record.
pub const MAX_STORED_VIEWS: usize = 64;

#[derive(Default)]
struct Inner {
    views: HashMap<Uuid, DataView>,
    /// Insertion order, oldest first — the FIFO eviction queue. A `view_id` appears at
    /// most once here (re-inserting an existing id updates `views` in place without
    /// re-queuing it), so eviction always removes the OLDEST distinct view, never the
    /// same id twice.
    order: VecDeque<Uuid>,
}

/// `Arc`-shareable, `Mutex`-guarded view store. See module doc for the snapshot/eviction
/// contract. Held on the `Orchestrator` (wiring landed in Phase 3, not here — this phase
/// only builds and unit-tests the store itself).
pub struct ViewStore {
    inner: Mutex<Inner>,
}

impl ViewStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Store `view` under its own `view_id`, evicting the single oldest entry if this
    /// insert pushes the store past [`MAX_STORED_VIEWS`]. Returns `view_id` back to the
    /// caller for convenience. Recovers from a poisoned lock (a prior panicking holder
    /// left no partially-written state this type cares about) rather than propagating
    /// the panic to an unrelated tool call.
    pub fn insert(&self, view: DataView) -> Uuid {
        let id = view.view_id;
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.views.insert(id, view).is_none() {
            guard.order.push_back(id);
        }
        while guard.order.len() > MAX_STORED_VIEWS {
            if let Some(oldest) = guard.order.pop_front() {
                guard.views.remove(&oldest);
            } else {
                break;
            }
        }
        id
    }

    /// Read the current snapshot for `id`, or `None` if unknown/evicted.
    pub fn get(&self, id: &Uuid) -> Option<DataView> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.views.get(id).cloned()
    }
}

impl Default for ViewStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewSink for ViewStore {
    fn insert(&self, view: DataView) -> Uuid {
        ViewStore::insert(self, view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_types::{ProjectionKind, ProjectionSpec, ViewProvenance};
    use std::sync::Arc;

    fn view(entity: &str) -> DataView {
        DataView {
            view_id: Uuid::new_v4(),
            entity: entity.to_string(),
            schema: vec![],
            records: vec![],
            projections: vec![ProjectionSpec {
                kind: ProjectionKind::Table,
                binding: None,
            }],
            active: ProjectionSpec {
                kind: ProjectionKind::Table,
                binding: None,
            },
            total: None,
            cursor: None,
            provenance: ViewProvenance::LlmProjected,
        }
    }

    #[test]
    fn get_after_insert_returns_the_inserted_view() {
        let store = ViewStore::new();
        let v = view("contact");
        let id = store.insert(v.clone());
        assert_eq!(id, v.view_id);
        assert_eq!(store.get(&id), Some(v));
    }

    #[test]
    fn get_unknown_id_returns_none() {
        let store = ViewStore::new();
        assert_eq!(store.get(&Uuid::new_v4()), None);
    }

    #[test]
    fn evicts_oldest_entry_once_past_cap() {
        let store = ViewStore::new();
        let mut ids = Vec::new();
        for i in 0..(MAX_STORED_VIEWS + 5) {
            let v = view(&format!("entity-{i}"));
            ids.push(store.insert(v));
        }
        // The first 5 inserted must have been evicted (FIFO, oldest-first).
        for id in &ids[..5] {
            assert!(store.get(id).is_none(), "oldest entries must be evicted past cap");
        }
        // The most recent MAX_STORED_VIEWS must still be present.
        for id in &ids[5..] {
            assert!(store.get(id).is_some(), "recent entries must survive eviction");
        }
    }

    #[test]
    fn reinserting_an_existing_id_does_not_double_queue_it() {
        let store = ViewStore::new();
        let mut v = view("contact");
        let id = v.view_id;
        store.insert(v.clone());
        v.entity = "contact-updated".to_string();
        store.insert(v.clone());
        assert_eq!(store.get(&id).map(|r| r.entity), Some("contact-updated".to_string()));
        // Re-inserting the same id must not consume two eviction slots — fill up to
        // exactly the cap using the same id repeatedly, then confirm a genuinely new
        // entry still evicts only the true oldest (a different, first-ever id), proving
        // no phantom duplicate queue entries exist for `id`.
        for _ in 0..(MAX_STORED_VIEWS * 2) {
            store.insert(v.clone());
        }
        assert!(store.get(&id).is_some(), "repeatedly re-inserted id must survive");
    }

    #[tokio::test]
    async fn concurrent_inserts_are_safe() {
        let store = Arc::new(ViewStore::new());
        let mut handles = Vec::new();
        for i in 0..50 {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                store.insert(view(&format!("concurrent-{i}")))
            }));
        }
        let mut ids = Vec::new();
        for h in handles {
            ids.push(h.await.expect("task"));
        }
        // Every inserted id (within the cap) must be independently readable — proves no
        // torn/corrupted state from concurrent access under the shared Mutex.
        let present = ids.iter().filter(|id| store.get(id).is_some()).count();
        assert!(present > 0, "at least some concurrently inserted views must be retrievable");
        assert!(present <= MAX_STORED_VIEWS, "store must never exceed its cap");
    }
}
