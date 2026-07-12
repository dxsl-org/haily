//! Real-DB, real-`AdapterManager` tests for `run_tick` — proves each MVP condition
//! fires exactly once per day (cooldown) and that the cooldown is PERSISTENT (survives
//! a simulated daemon restart), closing the in-process-`HashSet` regression this phase
//! replaced.
use super::super::{run_tick, run_tick_at};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{Duration, TimeZone, Utc};
use haily_db::{
    queries::{
        calendar::{self, NewCalendarEvent},
        tasks,
    },
    DbHandle,
};
use haily_io::{Adapter, AdapterManager, Notification, RequestSender, ResponseChunk};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Records every `notify()` call instead of delivering anywhere — lets tests assert
/// exactly which nudges fired (and, just as importantly, which did NOT).
struct RecordingAdapter {
    notifications: Arc<Mutex<Vec<Notification>>>,
}

#[async_trait]
impl Adapter for RecordingAdapter {
    async fn start(&self, _tx: RequestSender) -> Result<()> {
        Ok(())
    }
    async fn deliver(&self, _session_id: Uuid, _chunk: ResponseChunk) -> Result<()> {
        Ok(())
    }
    async fn notify(&self, msg: Notification) -> Result<()> {
        self.notifications.lock().expect("lock").push(msg);
        Ok(())
    }
    fn id(&self) -> &str {
        "recording"
    }
}

fn adapter_manager() -> (AdapterManager, Arc<Mutex<Vec<Notification>>>) {
    let notifications = Arc::new(Mutex::new(Vec::new()));
    let am = AdapterManager::builder()
        .register(Arc::new(RecordingAdapter { notifications: notifications.clone() }))
        .build();
    (am, notifications)
}

fn titles(notifications: &Mutex<Vec<Notification>>) -> Vec<String> {
    notifications
        .lock()
        .expect("lock")
        .iter()
        .map(|n| match n {
            Notification::Alert { title, .. } => title.clone(),
            other => panic!("unexpected notification variant: {other:?}"),
        })
        .collect()
}

async fn insert_event(db: &DbHandle, title: &str, start_at: &str) -> calendar::CalendarEvent {
    calendar::insert(
        db,
        NewCalendarEvent {
            title,
            description: None,
            location: None,
            start_at,
            end_at: start_at,
            all_day: false,
            recurrence: None,
        },
    )
    .await
    .expect("insert event")
}

async fn insert_task(db: &DbHandle, title: &str, due_at: Option<&str>) -> tasks::Task {
    tasks::insert(db, title, None, "medium", due_at, None).await.expect("insert task")
}

async fn link(db: &DbHandle, task_id: &str, event_id: &str) {
    sqlx::query("UPDATE tasks SET calendar_event_id = ? WHERE id = ?")
        .bind(event_id)
        .bind(task_id)
        .execute(db.pool())
        .await
        .expect("link task to event");
}

#[tokio::test]
async fn each_mvp_condition_fires_once_and_a_same_session_second_tick_is_suppressed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = DbHandle::init(&dir.path().join("cross.db")).await.expect("db init");
    // Fixed mid-day `now` (not `Utc::now()`): the "later today" condition filters events by
    // same-UTC-date prefix, so a real clock in the last hours of a UTC day would push the
    // `now + 3h` fixture into tomorrow and drop one nudge. Noon keeps every offset in-day.
    let now = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).single().expect("valid instant");

    let prep_event = insert_event(&db, "Standup", &(now + Duration::minutes(5)).to_rfc3339()).await;
    let prep_task = insert_task(&db, "Chuẩn bị slide", None).await;
    link(&db, &prep_task.id, &prep_event.id).await;

    insert_event(&db, "1:1", &(now + Duration::minutes(8)).to_rfc3339()).await;
    insert_event(&db, "Client demo", &(now + Duration::hours(3)).to_rfc3339()).await;
    insert_task(&db, "Việc trễ hạn", Some(&(now - Duration::days(1)).to_rfc3339())).await;

    let blocked_event =
        insert_event(&db, "Board review", &(now + Duration::days(5)).to_rfc3339()).await;
    let blocking_task =
        insert_task(&db, "Báo cáo quý", Some(&(now - Duration::days(1)).to_rfc3339())).await;
    link(&db, &blocking_task.id, &blocked_event.id).await;

    let (am, notifications) = adapter_manager();
    run_tick_at(&db, &am, now).await;

    let fired = titles(&notifications);
    assert_eq!(fired.len(), 5, "expected exactly one nudge per condition, got {fired:?}");

    run_tick_at(&db, &am, now).await;
    assert_eq!(
        titles(&notifications).len(),
        5,
        "same-session second tick must not re-fire any already-claimed condition"
    );
}

/// Restart-survival: a fresh `DbHandle` against the SAME file (simulating a daemon
/// restart, which reset the old in-process `HashSet`) must still see the earlier claim
/// and keep suppressing it — proving the ledger, not process memory, is the source of
/// truth.
#[tokio::test]
async fn cooldown_survives_a_simulated_daemon_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cross.db");
    let now = Utc::now();

    let db1 = DbHandle::init(&path).await.expect("db init");
    insert_task(&db1, "Việc trễ hạn", Some(&(now - Duration::days(1)).to_rfc3339())).await;
    let (am1, notifications1) = adapter_manager();
    run_tick(&db1, &am1).await;
    assert_eq!(titles(&notifications1), vec!["⚠️ Task quá hạn"]);
    drop(db1);

    let db2 = DbHandle::init(&path).await.expect("db init (restart)");
    let (am2, notifications2) = adapter_manager();
    run_tick(&db2, &am2).await;
    assert!(
        titles(&notifications2).is_empty(),
        "restart must not re-fire a condition already claimed before the restart"
    );
}
