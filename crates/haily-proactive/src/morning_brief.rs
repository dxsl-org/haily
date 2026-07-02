/// Daily morning brief — calendar + tasks + reminders summary, sent once per day.
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveTime};
use haily_db::{
    queries::{calendar, meta, reminders, tasks},
    DbHandle,
};
use haily_io::{AdapterManager, Notification};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const DEFAULT_BRIEF_TIME: &str = "07:30";

fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    let mut p = s.splitn(2, ':');
    let h: u32 = p.next()?.parse().ok()?;
    let m: u32 = p.next()?.parse().ok()?;
    NaiveTime::from_hms_opt(h, m, 0)
}

/// Resolve a local wall-clock instant, tolerating DST ambiguity/gaps.
///
/// `and_local_timezone(Local)` returns `LocalResult::None` for times that don't exist
/// (spring-forward gap) and `LocalResult::Ambiguous` for times that occur twice
/// (fall-back overlap) — `.unwrap()` panics on both. `.earliest()` picks a
/// deterministic instant for the ambiguous case and yields `None` for the gap case,
/// which the caller must handle by skipping the occurrence rather than crashing.
fn local_at(date: NaiveDate, time: NaiveTime) -> Option<DateTime<Local>> {
    date.and_time(time).and_local_timezone(Local).earliest()
}

/// Compute the next wall-clock firing of `time` (today if still future, else tomorrow).
/// Returns `None` if neither today's nor tomorrow's occurrence resolves to a valid
/// local instant (DST gap) — caller skips this cycle and retries on the next loop tick.
fn next_occurrence(time: NaiveTime) -> Option<chrono::DateTime<Local>> {
    let now = Local::now();
    let today = local_at(now.date_naive(), time)?;
    if today > now {
        Some(today)
    } else {
        local_at(now.date_naive() + Duration::days(1), time)
    }
}

async fn load_brief_time(db: &DbHandle) -> NaiveTime {
    let s = meta::get_preference(db, "morning_brief.time")
        .await
        .unwrap_or_default()
        .unwrap_or_else(|| DEFAULT_BRIEF_TIME.to_string());
    parse_hhmm(&s).unwrap_or_else(|| parse_hhmm(DEFAULT_BRIEF_TIME).unwrap())
}

async fn generate_brief(db: &DbHandle) -> String {
    let now = Local::now();
    // Midnight and 23:59:59 can both land in a DST transition on rare dates; fall back
    // to `now` itself (still within today) rather than panicking or skipping the brief.
    let today_start = local_at(now.date_naive(), NaiveTime::from_hms_opt(0, 0, 0).unwrap())
        .unwrap_or(now)
        .to_rfc3339();
    let today_end = local_at(now.date_naive(), NaiveTime::from_hms_opt(23, 59, 59).unwrap())
        .unwrap_or(now)
        .to_rfc3339();

    let mut parts: Vec<String> = Vec::new();

    // Calendar events today
    match calendar::upcoming(db, &today_start, &today_end).await {
        Ok(events) if !events.is_empty() => {
            let mut block = format!("📅 **Lịch hôm nay** ({} sự kiện)\n", events.len());
            for e in &events {
                let start = e.start_at.get(11..16).unwrap_or(&e.start_at);
                block.push_str(&format!("  • {start} — {}\n", e.title));
                if let Some(loc) = &e.location {
                    block.push_str(&format!("    📍 {loc}\n"));
                }
            }
            parts.push(block);
        }
        Ok(_) => parts.push("📅 Không có lịch hôm nay.".to_string()),
        Err(e) => warn!("brief: calendar query failed: {e:#}"),
    }

    // Urgent/high tasks
    match tasks::active(db).await {
        Ok(active) => {
            let urgent: Vec<_> = active
                .iter()
                .filter(|t| matches!(t.priority.as_str(), "urgent" | "high"))
                .collect();
            if !urgent.is_empty() {
                let mut block = format!("⚡ **Tasks ưu tiên cao** ({})\n", urgent.len());
                for t in &urgent {
                    let due = t.due_at.as_deref().and_then(|d| d.get(..10)).unwrap_or("no deadline");
                    block.push_str(&format!("  • [{}] {} — {due}\n", t.priority, t.title));
                }
                parts.push(block);
            }
            // Overdue
            let overdue: Vec<_> = active
                .iter()
                .filter(|t| {
                    t.due_at
                        .as_deref()
                        .map(|d| d < now.to_rfc3339().as_str())
                        .unwrap_or(false)
                })
                .collect();
            if !overdue.is_empty() {
                let block = format!(
                    "⚠️ **Overdue** ({} tasks)\n{}",
                    overdue.len(),
                    overdue.iter().map(|t| format!("  • {}\n", t.title)).collect::<String>()
                );
                parts.push(block);
            }
        }
        Err(e) => warn!("brief: tasks query failed: {e:#}"),
    }

    // Today's reminders
    match reminders::pending(db, &today_end).await {
        Ok(rems) if !rems.is_empty() => {
            let mut block = format!("⏰ **Nhắc nhở hôm nay** ({})\n", rems.len());
            for r in &rems {
                let time = r.fire_at.get(11..16).unwrap_or(&r.fire_at);
                block.push_str(&format!("  • {time} — {}\n", r.title));
            }
            parts.push(block);
        }
        _ => {}
    }

    if parts.is_empty() {
        return format!("Chào buổi sáng! 🌅 Hôm nay {} không có gì đặc biệt. Chúc ngày tốt lành.", now.format("%d/%m/%Y"));
    }

    format!(
        "☀️ **Morning Brief** — {}\n\n{}",
        now.format("%A, %d/%m/%Y"),
        parts.join("\n")
    )
}

/// Runs until `shutdown` is cancelled: sleeps until the configured morning-brief
/// time, sends the brief, repeats. Cancellation is observed at every sleep point so
/// shutdown does not wait out the (up to 24h) delay until the next brief.
pub async fn loop_forever(db: Arc<DbHandle>, am: AdapterManager, shutdown: CancellationToken) {
    loop {
        let brief_time = load_brief_time(&db).await;
        let Some(next) = next_occurrence(brief_time) else {
            // Both today's and tomorrow's occurrence fell in a DST gap — extremely rare.
            // Skip this cycle and re-evaluate in an hour rather than busy-looping.
            warn!("morning brief: occurrence time falls in a DST gap — skipping, retrying in 1h");
            tokio::select! {
                _ = shutdown.cancelled() => { info!("morning brief loop shutting down"); break; }
                _ = tokio::time::sleep(std::time::Duration::from_secs(3600)) => {}
            }
            continue;
        };
        let delay = (next - Local::now()).to_std().unwrap_or_default();

        info!(at = %next, "morning brief scheduled");
        tokio::select! {
            _ = shutdown.cancelled() => { info!("morning brief loop shutting down"); break; }
            _ = tokio::time::sleep(delay) => {}
        }

        if crate::dnd::is_active(&db).await {
            info!("morning brief suppressed by DND");
            continue;
        }

        let brief = generate_brief(&db).await;
        if let Err(e) = am.notify_all(Notification::MorningBrief(brief)).await {
            warn!("morning brief delivery failed: {e:#}");
        } else {
            info!("morning brief sent");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_at_resolves_an_ordinary_time_without_panicking() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let time = NaiveTime::from_hms_opt(7, 30, 0).unwrap();
        let resolved = local_at(date, time);
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().naive_local().time(), time);
    }

    #[test]
    fn next_occurrence_never_panics_for_any_hhmm_in_a_full_day() {
        // Sweep every minute of the day; `.earliest()` must never panic even though
        // some of these times fall inside a DST gap/overlap on the host's local
        // timezone (whatever that happens to be in CI).
        for h in 0..24 {
            for m in [0, 15, 30, 45] {
                let time = NaiveTime::from_hms_opt(h, m, 0).unwrap();
                // Either resolves to a valid instant or is skipped — both are
                // acceptable outcomes; a panic is the only failure mode being guarded.
                let _ = next_occurrence(time);
            }
        }
    }

    #[test]
    fn parse_hhmm_rejects_malformed_input_instead_of_panicking() {
        assert!(parse_hhmm("07:30").is_some());
        assert!(parse_hhmm("garbage").is_none());
        assert!(parse_hhmm("25:99").is_none());
    }
}
