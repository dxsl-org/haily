/// Cross-domain nudges — deterministic correlations across tasks, calendar events, and
/// reminders. Runs on a 5-minute tick; nudges are informational only and never mutate
/// state, per product-vision "ask before acting".
///
/// Cooldown/de-dup contract: each nudge is keyed by `(condition, entity_id, day)` and
/// claimed via `nudge_ledger::try_claim` (migration `0021`, `nudge_cooldown_ledger`) — an
/// atomic `INSERT OR IGNORE`, NOT the in-process `HashSet` this replaced. The old HashSet
/// reset on every daemon restart and re-spammed every still-open condition; the ledger
/// survives restarts, so a condition that already fired today stays suppressed even
/// across a crash/restart.
///
/// Detector logic (pure, DB/IO-free) lives in `detectors.rs` for unit testability.
mod detectors;
#[cfg(test)]
mod tests;

use chrono::{DateTime, Duration, Utc};
use detectors::{detect_event_no_prep, detect_meeting_conditions, detect_task_conditions};
use haily_db::{
    queries::{
        calendar::{self, CalendarEvent},
        nudge_ledger,
        tasks::{self, Task},
    },
    DbHandle,
};
use haily_io::{AdapterManager, Notification};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const POLL_INTERVAL_SECS: u64 = 300; // 5 minutes
const MEETING_PREP_MINS: i64 = 15;
/// Bounds the "future commitment" lookahead for the overdue-task detector — unbounded
/// would mean every overdue task with a calendar link stays a live candidate forever.
const COMMITMENT_HORIZON_DAYS: i64 = 30;

pub async fn alert_loop(db: Arc<DbHandle>, am: AdapterManager, shutdown: CancellationToken) {
    loop {
        if !crate::dnd::is_active(&db).await {
            run_tick(&db, &am).await;
        }

        tokio::select! {
            _ = shutdown.cancelled() => { info!("cross-domain alert loop shutting down"); break; }
            _ = tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)) => {}
        }
    }
}

/// Exposed `pub(crate)` (mirrors `morning_brief::generate_brief`'s test-entrypoint
/// pattern) so `tests.rs` can drive one tick directly against a real DB + mock adapter
/// without waiting out the 300s poll interval. Not `pub` at the crate root: `lib.rs`
/// declares `mod cross_domain;` (private, owned by a different phase), so an external
/// `tests/*.rs` integration test cannot name this path at all regardless of this
/// function's own visibility — see `tests.rs`'s module doc comment for the full
/// rationale for testing in-crate instead.
pub(crate) async fn run_tick(db: &DbHandle, am: &AdapterManager) {
    run_tick_at(db, am, Utc::now()).await;
}

/// Clock-injectable core of [`run_tick`]. Tests pass a fixed `now` so day-boundary
/// conditions (e.g. an event "later today", filtered by same-UTC-date prefix) are
/// deterministic regardless of the real wall-clock time the suite runs at — a `now` in the
/// last minutes of a UTC day would otherwise push a `now + hours` fixture into tomorrow and
/// silently drop the "today" nudge (a real flake this split removes).
pub(crate) async fn run_tick_at(db: &DbHandle, am: &AdapterManager, now: DateTime<Utc>) {
    let today = now.format("%Y-%m-%d").to_string();
    let now_rfc3339 = now.to_rfc3339();
    let imminent_end = (now + Duration::minutes(MEETING_PREP_MINS)).to_rfc3339();
    let horizon_end = (now + Duration::days(COMMITMENT_HORIZON_DAYS)).to_rfc3339();

    let upcoming_events = match calendar::upcoming(db, &now_rfc3339, &horizon_end).await {
        Ok(v) => v,
        Err(e) => {
            warn!("cross_domain: calendar query failed: {e:#}");
            return;
        }
    };
    let active_tasks = match tasks::active(db).await {
        Ok(v) => v,
        Err(e) => {
            warn!("cross_domain: tasks query failed: {e:#}");
            return;
        }
    };
    let linked_tasks = match tasks::linked_to_calendar(db).await {
        Ok(v) => v,
        Err(e) => {
            warn!("cross_domain: linked-task query failed: {e:#}");
            return;
        }
    };

    let imminent_events: Vec<CalendarEvent> =
        upcoming_events.iter().filter(|e| e.start_at <= imminent_end).cloned().collect();
    // Excludes events already in `imminent_events` — an imminent event with no linked
    // task is already covered by `detect_meeting_conditions`'s `COND_MEETING_IMMINENT`
    // fallback, so including it here too would double-alert on the same entity in the
    // same tick (the nudge-fatigue risk the phase plan calls out explicitly).
    let today_events: Vec<CalendarEvent> = upcoming_events
        .iter()
        .filter(|e| e.start_at.starts_with(&today) && e.start_at > imminent_end)
        .cloned()
        .collect();
    let overdue_tasks: Vec<&Task> = active_tasks
        .iter()
        .filter(|t| t.due_at.as_deref().map(|d| d < now_rfc3339.as_str()).unwrap_or(false))
        .collect();

    let mut candidates = detect_meeting_conditions(&imminent_events, &active_tasks);
    candidates.extend(detect_event_no_prep(&today_events, &linked_tasks));
    candidates.extend(detect_task_conditions(&overdue_tasks, &upcoming_events));

    for c in candidates {
        match nudge_ledger::try_claim(db, c.condition, &c.entity_id, &today).await {
            Ok(true) => {
                let notif =
                    Notification::Alert { title: c.title, body: c.body, urgent: c.urgent };
                if let Err(e) = am.notify_all(notif).await {
                    warn!("cross-domain nudge delivery failed: {e:#}");
                } else {
                    info!(condition = c.condition, entity_id = %c.entity_id, "cross-domain nudge fired");
                }
            }
            Ok(false) => {} // already fired today — cooldown suppresses it
            Err(e) => warn!("cross_domain: cooldown ledger claim failed: {e:#}"),
        }
    }
}
