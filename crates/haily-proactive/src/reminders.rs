/// Reminder polling loop — fires pending reminders, handles recurrence.
use chrono::{Duration, Utc};
use haily_db::{queries::reminders, DbHandle};
use haily_io::{AdapterManager, Notification};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

const POLL_INTERVAL_SECS: u64 = 60;

/// Parse a recurrence rule and compute the next `fire_at` after `base`.
/// Supported rules: "daily", "weekly". Everything else is treated as one-shot.
fn next_recurrence(base: &str, rule: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(base).ok()?;
    let next = match rule {
        "daily" => parsed + Duration::days(1),
        "weekly" => parsed + Duration::weeks(1),
        _ => return None,
    };
    Some(next.to_rfc3339())
}

/// Runs until `shutdown` is cancelled: polls every `POLL_INTERVAL_SECS` for due
/// reminders and fires them.
pub async fn poll_loop(db: Arc<DbHandle>, am: AdapterManager, shutdown: CancellationToken) {
    loop {
        let now = Utc::now().to_rfc3339();

        match reminders::pending(&db, &now).await {
            Ok(due) => {
                for r in due {
                    fire_reminder(&db, &am, &r).await;
                }
            }
            Err(e) => warn!("reminder poll failed: {e:#}"),
        }

        tokio::select! {
            _ = shutdown.cancelled() => { info!("reminder poll loop shutting down"); break; }
            _ = tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)) => {}
        }
    }
}

async fn fire_reminder(db: &DbHandle, am: &AdapterManager, r: &reminders::Reminder) {
    if crate::dnd::is_active(db).await {
        info!(id = %r.id, "reminder suppressed by DND");
        return;
    }

    let fired_at = Utc::now().to_rfc3339();
    if let Err(e) = reminders::mark_fired(db, &r.id, &fired_at).await {
        warn!(id = %r.id, "mark_fired failed: {e:#}");
        return;
    }

    let reminder_uuid = Uuid::parse_str(&r.id).unwrap_or_default();
    let notif = Notification::ReminderFired {
        reminder_id: reminder_uuid,
        title: r.title.clone(),
    };

    if let Err(e) = am.notify_all(notif).await {
        warn!(id = %r.id, "reminder delivery failed: {e:#}");
    } else {
        info!(id = %r.id, title = %r.title, "reminder fired");
    }

    // Reschedule recurring reminders
    if let Some(rule) = &r.recurrence {
        if let Some(next_fire) = next_recurrence(&r.fire_at, rule) {
            let session_ref = r.session_id.as_deref();
            match reminders::insert(db, &r.title, &next_fire, Some(rule), session_ref).await {
                Ok(next) => info!(id = %next.id, next_at = %next_fire, "reminder rescheduled"),
                Err(e) => warn!(id = %r.id, "reschedule failed: {e:#}"),
            }
        }
    }
}
