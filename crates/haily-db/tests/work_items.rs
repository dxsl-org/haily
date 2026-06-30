use haily_db::{
    queries::{sessions, work_items},
    DbHandle,
};

async fn setup() -> (DbHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = DbHandle::init(&db_path).await.unwrap();
    (db, dir)
}

async fn make_session(db: &DbHandle) -> String {
    sessions::create_session(db, "test-adapter", None)
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn create_returns_queued_item() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let item = work_items::create(&db, &sid, "do something").await.unwrap();

    assert_eq!(item.status, "queued");
    assert_eq!(item.title, "do something");
    assert_eq!(item.progress, 0);
    assert!(item.started_at.is_none());
    assert!(item.completed_at.is_none());
    assert!(item.error.is_none());
}

#[tokio::test]
async fn get_nonexistent_returns_none() {
    let (db, _dir) = setup().await;
    let result = work_items::get(&db, "no-such-id").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn full_lifecycle_queued_to_done() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let item = work_items::create(&db, &sid, "lifecycle task").await.unwrap();
    assert_eq!(item.status, "queued");

    work_items::start(&db, &item.id).await.unwrap();
    let running = work_items::get(&db, &item.id).await.unwrap().unwrap();
    assert_eq!(running.status, "running");
    assert!(running.started_at.is_some());

    work_items::checkpoint(&db, &item.id, Some("read_file"), 30, r#"{"tool_index":0}"#)
        .await
        .unwrap();
    let ckpt = work_items::get(&db, &item.id).await.unwrap().unwrap();
    assert_eq!(ckpt.phase.as_deref(), Some("read_file"));
    assert_eq!(ckpt.progress, 30);
    assert!(ckpt.checkpoint.is_some());

    work_items::complete(&db, &item.id).await.unwrap();
    let done = work_items::get(&db, &item.id).await.unwrap().unwrap();
    assert_eq!(done.status, "done");
    assert!(done.completed_at.is_some());
}

#[tokio::test]
async fn fail_records_error_message() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let item = work_items::create(&db, &sid, "will fail").await.unwrap();
    work_items::start(&db, &item.id).await.unwrap();
    work_items::fail(&db, &item.id, "network timeout").await.unwrap();

    let failed = work_items::get(&db, &item.id).await.unwrap().unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.error.as_deref(), Some("network timeout"));
}

#[tokio::test]
async fn list_active_excludes_terminal_statuses() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let running = work_items::create(&db, &sid, "running").await.unwrap();
    work_items::start(&db, &running.id).await.unwrap();

    let queued = work_items::create(&db, &sid, "queued").await.unwrap();

    let done_item = work_items::create(&db, &sid, "done").await.unwrap();
    work_items::start(&db, &done_item.id).await.unwrap();
    work_items::complete(&db, &done_item.id).await.unwrap();

    let failed_item = work_items::create(&db, &sid, "failed").await.unwrap();
    work_items::start(&db, &failed_item.id).await.unwrap();
    work_items::fail(&db, &failed_item.id, "err").await.unwrap();

    let active = work_items::list_active(&db).await.unwrap();
    let ids: Vec<&str> = active.iter().map(|i| i.id.as_str()).collect();

    assert!(ids.contains(&running.id.as_str()), "running should be active");
    assert!(ids.contains(&queued.id.as_str()), "queued should be active");
    assert!(!ids.contains(&done_item.id.as_str()), "done should not be active");
    assert!(!ids.contains(&failed_item.id.as_str()), "failed should not be active");
}

#[tokio::test]
async fn list_interrupted_shows_only_interrupted() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let a = work_items::create(&db, &sid, "task a").await.unwrap();
    work_items::start(&db, &a.id).await.unwrap();
    work_items::mark_interrupted(&db, &a.id).await.unwrap();

    let b = work_items::create(&db, &sid, "task b — still running").await.unwrap();
    work_items::start(&db, &b.id).await.unwrap();

    let interrupted = work_items::list_interrupted(&db).await.unwrap();
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0].id, a.id);
    assert_eq!(interrupted[0].status, "interrupted");
}

#[tokio::test]
async fn reset_stale_running_returns_affected_count() {
    let (db, _dir) = setup().await;
    let sid = make_session(&db).await;

    let a = work_items::create(&db, &sid, "stale a").await.unwrap();
    work_items::start(&db, &a.id).await.unwrap();
    let b = work_items::create(&db, &sid, "stale b").await.unwrap();
    work_items::start(&db, &b.id).await.unwrap();

    // Queued item — should not be touched
    let _q = work_items::create(&db, &sid, "queued").await.unwrap();

    let count = work_items::reset_stale_running(&db).await.unwrap();
    assert_eq!(count, 2);

    let interrupted = work_items::list_interrupted(&db).await.unwrap();
    let ids: Vec<&str> = interrupted.iter().map(|i| i.id.as_str()).collect();
    assert!(ids.contains(&a.id.as_str()));
    assert!(ids.contains(&b.id.as_str()));
}
