/// Integration tests for phase-08's HNSW lifecycle: tombstones, atomic swap,
/// dump/load persistence, torn-dump/count-mismatch fallback, and delta reconciliation.
use haily_db::{queries::facts, DbHandle};
use haily_kms::hnsw::{dump_dir, HnswIndex};
use haily_kms::KmsHandle;
use std::sync::Arc;

/// A cheap, deterministic "embedding" — cosine distance only cares about direction,
/// so a small set of near-orthogonal basis-ish vectors gives stable nearest-neighbour
/// ordering without needing the real fastembed model in these tests.
fn fake_embedding(seed: u64) -> Vec<f32> {
    let mut v = vec![0.0f32; 8];
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = ((seed as usize + i) % 7) as f32 + 1.0;
    }
    v
}

async fn seed_fact_with_embedding(db: &DbHandle, subject: &str, seed: u64) -> String {
    let blob: Vec<u8> = fake_embedding(seed).iter().flat_map(|f| f.to_le_bytes()).collect();
    let fact = facts::insert_fact(
        db,
        facts::NewFact {
            domain_id: "test",
            subject,
            predicate: "is",
            object: "seeded",
            source: "test",
            source_ref: None,
            embedding: Some(&blob),
        },
    )
    .await
    .expect("insert fact");
    fact.id
}

async fn test_db(dir: &std::path::Path) -> DbHandle {
    let db_path = dir.join("haily.db");
    DbHandle::init(&db_path).await.expect("db init")
}

// ---------------------------------------------------------------------------
// Critical: forget → immediate ANN exclusion (same process, no restart)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_fact_excludes_it_from_ann_search_immediately() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = test_db(dir.path()).await;
    let id = seed_fact_with_embedding(&db, "alpha", 1).await;
    let _other = seed_fact_with_embedding(&db, "beta", 2).await;

    let kms = Arc::new(KmsHandle::init(db, dir.path()).await.expect("kms init"));

    // Direct index-level check: forget_fact tombstones synchronously (the DB
    // soft-delete and the in-memory tombstone insert both complete before
    // `forget_fact` returns), so a search issued right after must already exclude it.
    let removed = kms.forget_fact(&id).await.expect("forget_fact");
    assert!(removed, "forget_fact must report the fact was found and removed");

    // Query the ANN layer directly with a dimension-matching vector: this is the
    // behavior under test ("excludes from ANN search"), and going through
    // `search_hybrid` would embed the query with the real 768-dim model under
    // `--features embeddings`, clashing with these synthetic 8-dim fixtures.
    let results = kms.search_ann_by_vector(&fake_embedding(1), 10).await;
    assert!(
        results.iter().all(|(rid, _)| rid != &id),
        "forgotten fact must not appear in ANN results in the same process, got: {results:?}"
    );
}

// ---------------------------------------------------------------------------
// Critical: dump → restart-load → search parity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dump_then_load_preserves_search_results_across_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = test_db(dir.path()).await;
    let id_a = seed_fact_with_embedding(&db, "vietnam-trip", 10).await;
    let _id_b = seed_fact_with_embedding(&db, "unrelated", 99).await;

    let kms = Arc::new(KmsHandle::init(db, dir.path()).await.expect("kms init"));
    kms.flush_index().await;

    // "Restart": build a fresh KmsHandle against the SAME db file and dump dir.
    let db2 = DbHandle::init(&dir.path().join("haily.db")).await.expect("db reopen");
    let kms2 = Arc::new(KmsHandle::init(db2, dir.path()).await.expect("kms init after restart"));

    let query_vec = fake_embedding(10);
    let results = kms2.search_ann_by_vector(&query_vec, 5).await;
    assert!(
        results.iter().any(|(id, _)| id == &id_a),
        "the fact present before dump must still be found by ANN after load, got: {results:?}"
    );
}

// ---------------------------------------------------------------------------
// Critical: corrupt dump → rebuild fallback, no crash
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_manifest_falls_back_to_rebuild_without_crashing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = test_db(dir.path()).await;
    let id = seed_fact_with_embedding(&db, "gamma", 3).await;

    let kms = Arc::new(KmsHandle::init(db, dir.path()).await.expect("kms init"));
    kms.flush_index().await;

    // Corrupt the manifest with garbage bytes — a torn/corrupted dump.
    let manifest_path = dump_dir(dir.path()).join("kms_manifest.json");
    std::fs::write(&manifest_path, b"{not valid json").expect("corrupt manifest");

    let db2 = DbHandle::init(&dir.path().join("haily.db")).await.expect("db reopen");
    let kms2 = KmsHandle::init(db2, dir.path())
        .await
        .expect("init must fall back to rebuild, not error/panic");

    // ANN-direct query (dimension-matching) — the fallback rebuild's job is to
    // repopulate the ANN index from the DB, which is what this asserts.
    let results = kms2.search_ann_by_vector(&fake_embedding(3), 10).await;
    assert!(
        results.iter().any(|(rid, _)| rid == &id),
        "rebuild fallback must still find the fact via a fresh DB rebuild, got: {results:?}"
    );
}

// ---------------------------------------------------------------------------
// Critical: truncated (torn) dump with valid header → count mismatch → rebuild
// ---------------------------------------------------------------------------

#[test]
fn count_mismatch_between_manifest_and_id_map_is_detected_as_a_load_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = HnswIndex::new();
    index.insert("fact-1", &fake_embedding(1));
    index.insert("fact-2", &fake_embedding(2));
    index.dump(dir.path(), "test-model").expect("dump");

    // Simulate a torn dump: manifest claims more vectors than the id_map sidecar
    // actually has (as if the id_map write completed but the process died before
    // the manifest was finalized to match, or vice versa).
    let manifest_path = dir.path().join("kms_manifest.json");
    let raw = std::fs::read_to_string(&manifest_path).expect("read manifest");
    let mut manifest: serde_json::Value = serde_json::from_str(&raw).expect("parse manifest");
    manifest["count"] = serde_json::json!(999);
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).expect("rewrite manifest");

    let result = HnswIndex::load(dir.path());
    assert!(
        result.is_err(),
        "a manifest/id_map count mismatch must surface as an error (caller falls back to rebuild)"
    );
}

// ---------------------------------------------------------------------------
// High: delta reconciliation — fact added after dump is findable after load
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fact_created_after_dump_is_findable_after_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = test_db(dir.path()).await;
    let _id_a = seed_fact_with_embedding(&db, "before-dump", 20).await;

    let kms = Arc::new(KmsHandle::init(db.clone(), dir.path()).await.expect("kms init"));
    kms.flush_index().await;

    // Fact created strictly after the dump timestamp — RFC3339 string comparison
    // needs a real (even if tiny) time gap to be unambiguous.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let id_new = seed_fact_with_embedding(&db, "after-dump", 21).await;

    let db2 = DbHandle::init(&dir.path().join("haily.db")).await.expect("db reopen");
    let kms2 = Arc::new(KmsHandle::init(db2, dir.path()).await.expect("kms init after restart"));

    let results = kms2.search_ann_by_vector(&fake_embedding(21), 5).await;
    assert!(
        results.iter().any(|(id, _)| id == &id_new),
        "a fact created after the dump must be inserted via delta reconciliation on load, got: {results:?}"
    );
}

// ---------------------------------------------------------------------------
// High / Medium: rebuild triggers at >20% tombstones; swap is atomic
// ---------------------------------------------------------------------------

#[test]
fn should_rebuild_fires_past_the_20_percent_tombstone_watermark() {
    let index = HnswIndex::new();
    for i in 0..10 {
        index.insert(&format!("fact-{i}"), &fake_embedding(i as u64));
    }
    assert!(!index.should_rebuild(), "a fresh index with no tombstones must not need a rebuild");

    // 2/10 = 20% exactly — the trigger is "> 20%", not ">=", so this must not fire yet.
    index.tombstone("fact-0");
    index.tombstone("fact-1");
    assert!(!index.should_rebuild(), "exactly 20% tombstoned must not cross the '> 20%' watermark");

    // 3/10 = 30% — past the watermark.
    index.tombstone("fact-2");
    assert!(index.should_rebuild(), "tombstone ratio past 20% must trigger a rebuild");
}

// ---------------------------------------------------------------------------
// Phase 12 (memory-undo via KmsHandle compensator): `contains`/`un_tombstone`
// primitives — the branch `KmsHandle::restore_fact` uses to decide un-tombstone
// (id still present) vs. re-insert-from-BLOB (a compaction already dropped it).
// ---------------------------------------------------------------------------

#[test]
fn contains_is_true_only_for_an_actually_inserted_id() {
    let index = HnswIndex::new();
    assert!(!index.contains("fact-0"), "an id never inserted must report absent");
    index.insert("fact-0", &fake_embedding(0));
    assert!(index.contains("fact-0"));
    assert!(!index.contains("fact-1"), "a DIFFERENT id must still report absent");
}

#[test]
fn un_tombstone_clears_visibility_without_touching_id_map() {
    let index = HnswIndex::new();
    index.insert("fact-0", &fake_embedding(0));
    let len_before = index.len();

    index.tombstone("fact-0");
    assert!(index.is_tombstoned("fact-0"));
    assert!(
        index.contains("fact-0"),
        "tombstoning must not remove the id from id_map — contains() stays true"
    );

    index.un_tombstone("fact-0");
    assert!(!index.is_tombstoned("fact-0"));
    assert!(
        index.contains("fact-0"),
        "un_tombstone must not touch id_map either — contains() still true"
    );
    assert_eq!(
        index.len(),
        len_before,
        "un_tombstone must never grow id_map — no duplicate entry is ever created"
    );

    let results = index.search(&fake_embedding(0), 5);
    assert_eq!(
        results.iter().filter(|(id, _)| id == "fact-0").count(),
        1,
        "the id must be searchable exactly once after un_tombstone: {results:?}"
    );
}

#[tokio::test]
async fn search_never_errors_during_a_concurrent_background_rebuild() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = test_db(dir.path()).await;
    for i in 0..12u64 {
        seed_fact_with_embedding(&db, &format!("fact-{i}"), i).await;
    }

    let kms = Arc::new(KmsHandle::init(db, dir.path()).await.expect("kms init"));

    // Tombstone enough facts to cross the 20% watermark and trigger a background
    // rebuild-and-swap, then hammer search concurrently — the atomic `RwLock<Arc<_>>`
    // swap (not a bare `Arc` held by value) means every in-flight search either sees
    // the old or the new index, never a torn/partial one.
    for i in 0..4u64 {
        let ids = facts::list_by_domain(kms.db(), "test", 100).await.expect("list");
        if let Some(target) = ids.iter().find(|f| f.subject == format!("fact-{i}")) {
            kms.forget_fact(&target.id).await.expect("forget");
        }
    }

    // ANN-direct queries (dimension-matching) hammer the RwLock<Arc<_>> during the
    // tombstone-triggered background rebuild-and-swap. The property under test is that
    // no concurrent search observes a torn index and panics — so a clean task join is
    // the assertion.
    let mut handles = Vec::new();
    for _ in 0..20 {
        let kms = Arc::clone(&kms);
        handles.push(tokio::spawn(async move {
            kms.search_ann_by_vector(&fake_embedding(5), 5).await
        }));
    }
    for h in handles {
        assert!(
            h.await.is_ok(),
            "search must never panic (torn read) during a concurrent background rebuild/swap"
        );
    }
}
