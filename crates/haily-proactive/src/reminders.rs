/// Reminder polling loop — fires pending reminders, handles recurrence.
use chrono::{DateTime, Utc};
use haily_db::{queries::reminders, recurrence, DbHandle};
use haily_io::{AdapterManager, Notification};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

const POLL_INTERVAL_SECS: u64 = 60;

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

    let now = Utc::now();
    let fired_at = now.to_rfc3339();
    if !mark_fired_and_maybe_reschedule(db, r, &fired_at, now).await {
        // A DB error already logged inside — the fire itself did not commit, so no
        // notification goes out (nothing to tell the user actually happened).
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
}

/// Marks `r` fired and, for a recurring reminder, atomically inserts its next occurrence in
/// the SAME transaction via `reminders::mark_fired_and_reschedule` — `recurrence::next_after`
/// computes a `fire_at` strictly AFTER `now`, coalescing any backlog of missed occurrences
/// into that one row (never a drip-storm of one-fire-per-poll on wake).
///
/// Returns `false` only on a DB error (caller must not notify — the fire never committed). An
/// unparseable/degenerate stored rule (should not occur; rules are validated at write time)
/// degrades to a plain one-shot `mark_fired` rather than blocking the fire.
async fn mark_fired_and_maybe_reschedule(
    db: &DbHandle,
    r: &reminders::Reminder,
    fired_at: &str,
    now: DateTime<Utc>,
) -> bool {
    let Some(rule) = r.recurrence.as_deref() else {
        return mark_fired_ok(db, &r.id, fired_at).await;
    };

    match recurrence::next_after(rule, &r.fire_at, now) {
        Ok(Some(next_fire)) => {
            let next_id = Uuid::new_v4().to_string();
            match reminders::mark_fired_and_reschedule(
                db,
                &r.id,
                fired_at,
                &next_id,
                &next_fire,
                rule,
                &r.title,
                r.session_id.as_deref(),
            )
            .await
            {
                Ok(next) => {
                    info!(id = %next.id, next_at = %next_fire, "reminder rescheduled");
                    true
                }
                Err(e) => {
                    warn!(id = %r.id, "mark_fired_and_reschedule failed: {e:#}");
                    false
                }
            }
        }
        Ok(None) => {
            warn!(id = %r.id, rule = %rule, "recurrence rule unparseable — firing as one-shot");
            mark_fired_ok(db, &r.id, fired_at).await
        }
        Err(e) => {
            warn!(id = %r.id, "recurrence next_after failed: {e:#} — firing as one-shot");
            mark_fired_ok(db, &r.id, fired_at).await
        }
    }
}

async fn mark_fired_ok(db: &DbHandle, id: &str, fired_at: &str) -> bool {
    match reminders::mark_fired(db, id, fired_at).await {
        Ok(()) => true,
        Err(e) => {
            warn!(id = %id, "mark_fired failed: {e:#}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (DbHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        (db, dir)
    }

    /// CRITICAL forward-progress contract at the integration level: a daily reminder whose
    /// `fire_at` is a week in the past (daemon asleep) must fire exactly ONCE when the poll
    /// loop catches up — proven by asserting the rescheduled occurrence is no longer due, so
    /// an immediate second poll would NOT re-fire it. The old bug: `next_recurrence` advanced
    /// only one day past the ORIGINAL `fire_at`, which stayed `<= now` for the whole backlog,
    /// re-firing once per poll until caught up (a drip-storm).
    #[tokio::test]
    async fn week_old_daily_reminder_fires_once_and_is_no_longer_due() {
        let (db, _d) = db().await;
        let stale_fire_at = (Utc::now() - chrono::Duration::days(8)).to_rfc3339();
        let r = reminders::insert(&db, "Stale reminder", &stale_fire_at, Some("daily"), None)
            .await
            .unwrap();

        let now = Utc::now();
        let ok = mark_fired_and_maybe_reschedule(&db, &r, &now.to_rfc3339(), now).await;
        assert!(ok, "the fire+reschedule must succeed");

        let due_after = reminders::pending(&db, &now.to_rfc3339()).await.unwrap();
        assert!(
            due_after.is_empty(),
            "the rescheduled occurrence must be strictly in the future — an immediate \
             second poll must find nothing due (no drip-storm)"
        );

        let all = reminders::list_all(&db).await.unwrap();
        let rescheduled = all.iter().find(|x| x.id != r.id).expect("a new occurrence row exists");
        assert!(
            DateTime::parse_from_rfc3339(&rescheduled.fire_at).unwrap() > now,
            "next occurrence must be strictly after now"
        );
    }

    #[tokio::test]
    async fn non_recurring_reminder_just_marks_fired_without_inserting_a_successor() {
        let (db, _d) = db().await;
        let r = reminders::insert(&db, "One-shot", &Utc::now().to_rfc3339(), None, None)
            .await
            .unwrap();
        let now = Utc::now();
        let ok = mark_fired_and_maybe_reschedule(&db, &r, &now.to_rfc3339(), now).await;
        assert!(ok);

        let all = reminders::list_all(&db).await.unwrap();
        assert_eq!(all.len(), 1, "a non-recurring reminder must not spawn a successor row");
    }
}
