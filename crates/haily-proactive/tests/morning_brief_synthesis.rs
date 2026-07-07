//! Integration tests for the synthesized morning brief (Phase 3, "assistant-depth").
//!
//! Proves `generate_brief` correlates cross-source items — task<->calendar link,
//! overdue task<->note wikilink, reminder<->task deadline — instead of emitting four
//! independent lists, and that floored memory recall (Phase 1's `search_hybrid`)
//! surfaces an on-topic fact while an unrelated one never appears. No LLM is
//! involved anywhere in this file — every assertion is a deterministic substring
//! check against real DB-backed data.
use chrono::{Duration, Local, Utc};
use haily_db::{
    queries::{
        calendar::{self, NewCalendarEvent},
        reminders, tasks,
    },
    DbHandle,
};
use haily_kms::KmsHandle;
use haily_proactive::morning_brief::generate_brief;

async fn setup() -> (DbHandle, KmsHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = DbHandle::init(&dir.path().join("brief.db"))
        .await
        .expect("db init");
    let kms = KmsHandle::init(db.clone(), dir.path())
        .await
        .expect("kms init");
    (db, kms, dir)
}

/// Set a task's `calendar_event_id` FK directly. No query helper exists for this in
/// `haily-db` today — that column is normally populated via the local-mutation write
/// path exercised elsewhere — so the test writes it with a raw `UPDATE` instead of
/// adding a new production query surface just for a fixture.
async fn link_task_to_event(db: &DbHandle, task_id: &str, event_id: &str) {
    sqlx::query("UPDATE tasks SET calendar_event_id = ? WHERE id = ?")
        .bind(event_id)
        .bind(task_id)
        .execute(db.pool())
        .await
        .expect("link task to event");
}

#[tokio::test]
async fn brief_correlates_a_task_due_today_with_its_calendar_event() {
    let (db, kms, _dir) = setup().await;
    let now = Local::now();
    let start = now.to_rfc3339();
    let end = (now + Duration::hours(1)).to_rfc3339();

    let event = calendar::insert(
        &db,
        NewCalendarEvent {
            title: "Họp nhóm dự án",
            description: None,
            location: None,
            start_at: &start,
            end_at: &end,
            all_day: false,
            recurrence: None,
        },
    )
    .await
    .expect("insert event");

    let due = now.to_rfc3339();
    let task = tasks::insert(&db, "Chuẩn bị slide họp", None, "medium", Some(&due), None)
        .await
        .expect("insert task");
    link_task_to_event(&db, &task.id, &event.id).await;

    let brief = generate_brief(&db, &kms).await;
    assert!(
        brief.contains("Họp nhóm dự án"),
        "brief must show the calendar event:\n{brief}"
    );
    assert!(
        brief.contains("🔗 Task liên quan: Chuẩn bị slide họp"),
        "brief must correlate the task with its calendar event, not list them separately:\n{brief}"
    );
}

#[tokio::test]
async fn brief_flags_an_overdue_task_that_references_a_note() {
    let (db, kms, _dir) = setup().await;
    let past = (Utc::now() - Duration::days(2)).to_rfc3339();
    tasks::insert(
        &db,
        "Nộp báo cáo thuế",
        Some("xem [[Hướng dẫn thuế]] trước khi nộp"),
        "high",
        Some(&past),
        None,
    )
    .await
    .expect("insert overdue task");

    let brief = generate_brief(&db, &kms).await;
    assert!(
        brief.contains("Nộp báo cáo thuế"),
        "brief must list the overdue task:\n{brief}"
    );
    assert!(
        brief.contains("📝"),
        "brief must flag the task's wikilink reference to a note:\n{brief}"
    );
}

#[tokio::test]
async fn brief_does_not_flag_an_overdue_task_without_a_wikilink() {
    let (db, kms, _dir) = setup().await;
    let past = (Utc::now() - Duration::days(2)).to_rfc3339();
    tasks::insert(
        &db,
        "Dọn dẹp bàn làm việc",
        None,
        "medium",
        Some(&past),
        None,
    )
    .await
    .expect("insert overdue task");

    let brief = generate_brief(&db, &kms).await;
    assert!(brief.contains("Dọn dẹp bàn làm việc"));
    assert!(
        !brief.contains("📝"),
        "no wikilink present — must not be flagged:\n{brief}"
    );
}

#[tokio::test]
async fn brief_links_a_reminder_to_a_matching_task_deadline() {
    let (db, kms, _dir) = setup().await;
    let due = Local::now().to_rfc3339();
    tasks::insert(&db, "Nộp báo cáo thuế", None, "medium", Some(&due), None)
        .await
        .expect("insert task");
    reminders::insert(
        &db,
        "Nhắc nộp báo cáo thuế trước 5 giờ chiều",
        &due,
        None,
        None,
    )
    .await
    .expect("insert reminder");

    let brief = generate_brief(&db, &kms).await;
    assert!(
        brief.contains("🔗 Nộp báo cáo thuế"),
        "brief must link the reminder to the task sharing its deadline/title:\n{brief}"
    );
}

#[tokio::test]
async fn brief_includes_above_floor_memory_and_excludes_unrelated_facts() {
    let (db, kms, _dir) = setup().await;
    let due = Local::now().to_rfc3339();
    tasks::insert(
        &db,
        "Họp nhóm dự án Alpha",
        None,
        "medium",
        Some(&due),
        None,
    )
    .await
    .expect("insert task");

    kms.remember(
        "personal",
        "user",
        "quan tâm đến",
        "dự án Alpha và deadline sắp tới",
        "test",
        None,
    )
    .await
    .expect("remember on-topic fact");
    kms.remember(
        "personal",
        "user",
        "thích",
        "cà phê đen mỗi sáng",
        "test",
        None,
    )
    .await
    .expect("remember off-topic fact");

    let brief = generate_brief(&db, &kms).await;
    assert!(
        brief.contains("dự án Alpha và deadline sắp tới"),
        "on-topic fact sharing a theme word with today's task must appear:\n{brief}"
    );
    assert!(
        !brief.contains("cà phê đen"),
        "fact sharing no vocabulary with today's items must not be forced into the brief:\n{brief}"
    );
}

#[tokio::test]
async fn brief_falls_back_to_a_plain_greeting_on_a_totally_empty_day() {
    let (db, kms, _dir) = setup().await;
    let brief = generate_brief(&db, &kms).await;
    assert!(
        brief.contains("Chào buổi sáng"),
        "empty day must still produce a friendly brief:\n{brief}"
    );
}
