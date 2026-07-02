/// Cross-domain alerts — upcoming meeting prep and overdue task nudges.
///
/// Runs on a 5-minute tick. Fires once per item per daemon session using an
/// in-process HashSet so the user isn't spammed across multiple restarts.
use chrono::{Duration, Utc};
use haily_db::{queries::{calendar, tasks}, DbHandle};
use haily_io::{AdapterManager, Notification};
use std::{collections::HashSet, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const POLL_INTERVAL_SECS: u64 = 300; // 5 minutes
const MEETING_PREP_MINS: i64 = 15;

pub async fn alert_loop(db: Arc<DbHandle>, am: AdapterManager, shutdown: CancellationToken) {
    let alerted_events: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let alerted_tasks: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    loop {
        if !crate::dnd::is_active(&db).await {
            check_upcoming_meetings(&db, &am, &alerted_events).await;
            check_overdue_tasks(&db, &am, &alerted_tasks).await;
        }

        tokio::select! {
            _ = shutdown.cancelled() => { info!("cross-domain alert loop shutting down"); break; }
            _ = tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)) => {}
        }
    }
}

async fn check_upcoming_meetings(
    db: &DbHandle,
    am: &AdapterManager,
    alerted: &Mutex<HashSet<String>>,
) {
    let now = Utc::now();
    let window_start = now.to_rfc3339();
    let window_end = (now + Duration::minutes(MEETING_PREP_MINS)).to_rfc3339();

    let events = match calendar::upcoming(db, &window_start, &window_end).await {
        Ok(v) => v,
        Err(e) => {
            warn!("cross_domain: calendar query failed: {e:#}");
            return;
        }
    };

    let mut seen = alerted.lock().await;
    for e in events {
        if seen.contains(&e.id) {
            continue;
        }
        seen.insert(e.id.clone());

        let start_time = e.start_at.get(11..16).unwrap_or(&e.start_at);
        let body = if let Some(loc) = &e.location {
            format!("{} lúc {} tại {}", e.title, start_time, loc)
        } else {
            format!("{} lúc {}", e.title, start_time)
        };

        let notif = Notification::Alert {
            title: "📅 Sắp có cuộc họp".to_string(),
            body,
            urgent: false,
        };

        if let Err(e) = am.notify_all(notif).await {
            warn!("meeting alert delivery failed: {e:#}");
        } else {
            info!(event_id = %e.id, "meeting prep alert fired");
        }
    }
}

async fn check_overdue_tasks(
    db: &DbHandle,
    am: &AdapterManager,
    alerted: &Mutex<HashSet<String>>,
) {
    let now = Utc::now().to_rfc3339();

    let active = match tasks::active(db).await {
        Ok(v) => v,
        Err(e) => {
            warn!("cross_domain: tasks query failed: {e:#}");
            return;
        }
    };

    let overdue: Vec<_> = active
        .into_iter()
        .filter(|t| t.due_at.as_deref().map(|d| d < now.as_str()).unwrap_or(false))
        .collect();

    if overdue.is_empty() {
        return;
    }

    let mut seen = alerted.lock().await;
    for t in overdue {
        if seen.contains(&t.id) {
            continue;
        }
        seen.insert(t.id.clone());

        let due_date = t.due_at.as_deref().and_then(|d| d.get(..10)).unwrap_or("?");
        let notif = Notification::Alert {
            title: "⚠️ Task quá hạn".to_string(),
            body: format!("[{}] {} — hạn {}", t.priority, t.title, due_date),
            urgent: matches!(t.priority.as_str(), "urgent" | "high"),
        };

        if let Err(e) = am.notify_all(notif).await {
            warn!("overdue task alert delivery failed: {e:#}");
        } else {
            info!(task_id = %t.id, "overdue task alert fired");
        }
    }
}
