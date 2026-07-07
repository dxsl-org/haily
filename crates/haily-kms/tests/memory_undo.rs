//! Phase 12 (memory-undo via KmsHandle compensator): `KmsHandle::restore_fact` primitive
//! coverage — the un-tombstone (no-compaction) path, the re-insert-from-BLOB
//! (post-compaction) path, and the C2 restore-vs-rebuild race. The higher-level
//! single/batch/turn `journal_undo` integration (which needs the tool-layer machinery
//! this crate does not depend on) lives in `haily-tools`'s `journal_undo` test modules.
use haily_db::{queries::facts, DbHandle};
use haily_kms::KmsHandle;
use std::sync::Arc;
use std::time::Duration;

/// A cheap, deterministic "embedding" — mirrors `hnsw_lifecycle.rs`'s own fixtures.
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
    DbHandle::init(&dir.join("haily.db")).await.expect("db init")
}

/// `updated_at` for a (possibly soft-deleted) row — `facts::get_fact` filters
/// `deleted_at IS NULL` and would hide it, but `restore_fact`'s C10 guard needs the
/// value AS OF the forget, before any restore.
async fn updated_at_of(db: &DbHandle, id: &str) -> String {
    facts::get_updated_at(db, id)
        .await
        .expect("query updated_at")
        .expect("fact must exist")
}

// ---------------------------------------------------------------------------
// No-compaction path: the id is still in `id_map` (only tombstoned) — restore must
// un-tombstone, never bare-insert (which would duplicate the id_map entry).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restore_fact_un_tombstones_when_no_compaction_happened() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = test_db(dir.path()).await;
    // Keep the tombstone ratio under the 20% auto-rebuild watermark (1/10) so this
    // KmsHandle's OWN index never gets swapped out from under the test.
    for i in 0..9u64 {
        seed_fact_with_embedding(&db, &format!("filler-{i}"), 100 + i).await;
    }
    let id = seed_fact_with_embedding(&db, "coffee", 1).await;

    let kms = Arc::new(KmsHandle::init(db.clone(), dir.path()).await.expect("kms init"));
    let removed = kms.forget_fact(&id).await.expect("forget_fact");
    assert!(removed);
    let expected_updated_at = updated_at_of(&db, &id).await;

    assert!(
        kms.search_ann_by_vector(&fake_embedding(1), 10)
            .await
            .iter()
            .all(|(rid, _)| rid != &id),
        "forgotten fact must not surface before undo"
    );

    let restored = kms
        .restore_fact(&id, &expected_updated_at)
        .await
        .expect("restore_fact");
    assert!(restored, "restore_fact must report success");

    let results = kms.search_ann_by_vector(&fake_embedding(1), 10).await;
    assert_eq!(
        results.iter().filter(|(rid, _)| rid == &id).count(),
        1,
        "the fact must appear EXACTLY once — a bare `insert` on a still-present id would \
         duplicate the id_map entry: {results:?}"
    );
}

// ---------------------------------------------------------------------------
// Post-compaction path: a "restart" builds a FRESH index straight from the DB, which
// naturally excludes a still-soft-deleted fact from its id_map — functionally identical
// to what a background compaction rebuild leaves behind. `restore_fact` must then
// re-insert from the stored embedding BLOB, never re-embed.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restore_fact_reinserts_from_blob_when_id_absent_post_compaction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = test_db(dir.path()).await;
    for i in 0..9u64 {
        seed_fact_with_embedding(&db, &format!("filler-{i}"), 200 + i).await;
    }
    let id = seed_fact_with_embedding(&db, "tea", 2).await;

    let kms1 = Arc::new(KmsHandle::init(db.clone(), dir.path()).await.expect("kms init"));
    kms1.forget_fact(&id).await.expect("forget_fact");
    let expected_updated_at = updated_at_of(&db, &id).await;
    drop(kms1);

    // A fresh KmsHandle's `rebuild_from_db` (no dump on disk yet) reads the DB fresh —
    // its id_map never contained `id` at all, matching what a compaction rebuild leaves.
    let db2 = DbHandle::init(&dir.path().join("haily.db")).await.expect("db reopen");
    let kms2 = Arc::new(KmsHandle::init(db2, dir.path()).await.expect("kms init after restart"));

    let restored = kms2
        .restore_fact(&id, &expected_updated_at)
        .await
        .expect("restore_fact");
    assert!(restored);

    let results = kms2.search_ann_by_vector(&fake_embedding(2), 10).await;
    assert!(
        results.iter().any(|(rid, _)| rid == &id),
        "post-compaction restore must re-insert from the stored BLOB: {results:?}"
    );
}

/// C10 parity: a stale `expected_updated_at` (the record changed under us) must refuse.
#[tokio::test]
async fn restore_fact_refuses_on_stale_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = test_db(dir.path()).await;
    for i in 0..9u64 {
        seed_fact_with_embedding(&db, &format!("filler-{i}"), 300 + i).await;
    }
    let id = seed_fact_with_embedding(&db, "milk", 3).await;

    let kms = Arc::new(KmsHandle::init(db.clone(), dir.path()).await.expect("kms init"));
    kms.forget_fact(&id).await.expect("forget_fact");

    let restored = kms
        .restore_fact(&id, "not-the-real-updated-at")
        .await
        .expect("restore_fact call itself must not error");
    assert!(!restored, "a stale/wrong version must refuse, not blindly restore");
}

// ---------------------------------------------------------------------------
// C2: a forget that crosses the tombstone ratio triggers a background rebuild; a
// restore racing that rebuild must survive the pointer swap (`ids_undeleted_since`).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restore_survives_a_concurrent_background_rebuild_c2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = test_db(dir.path()).await;
    let mut ids = Vec::new();
    for i in 0..10u64 {
        ids.push(seed_fact_with_embedding(&db, &format!("fact-{i}"), 400 + i).await);
    }

    let kms = Arc::new(KmsHandle::init(db.clone(), dir.path()).await.expect("kms init"));

    // Forget 2 facts (20% — at, not past, the watermark: no rebuild yet).
    kms.forget_fact(&ids[0]).await.expect("forget 0");
    kms.forget_fact(&ids[1]).await.expect("forget 1");
    // The 3rd forget crosses 20% (3/10 = 30%) and starts the background rebuild.
    kms.forget_fact(&ids[2]).await.expect("forget 2");
    let expected_updated_at = updated_at_of(&db, &ids[2]).await;

    // Yield so the background task gets a chance to capture its `since` boundary
    // before our restore commits — biasing toward the exact race C2 fixes, though the
    // fix must hold regardless of the precise interleaving.
    tokio::task::yield_now().await;
    let restored = kms
        .restore_fact(&ids[2], &expected_updated_at)
        .await
        .expect("restore_fact");
    assert!(restored);

    // Poll for the background rebuild to swap — without the `ids_undeleted_since`
    // delta, the swap would silently drop the just-restored fact.
    let mut found = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let results = kms.search_ann_by_vector(&fake_embedding(400 + 2), 20).await;
        found = results.iter().any(|(rid, _)| rid == &ids[2]);
    }
    assert!(
        found,
        "a fact restored while a background rebuild was in flight must survive the swap (C2)"
    );
}
