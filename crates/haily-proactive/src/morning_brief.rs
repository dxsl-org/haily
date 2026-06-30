/// Daily morning brief — calendar + tasks + reminders summary, sent once per day.
use chrono::{Duration, Local, NaiveTime};
use haily_db::{
    queries::{calendar, meta, reminders, tasks},
    DbHandle,
};
use haily_io::{AdapterManager, Notification};
use std::sync::Arc;
use tracing::{info, warn};

const DEFAULT_BRIEF_TIME: &str = "07:30";

fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    let mut p = s.splitn(2, ':');
    let h: u32 = p.next()?.parse().ok()?;
    let m: u32 = p.next()?.parse().ok()?;
    NaiveTime::from_hms_opt(h, m, 0)
}

/// Compute the next wall-clock firing of `time` (today if still future, else tomorrow).
fn next_occurrence(time: NaiveTime) -> chrono::DateTime<Local> {
    let now = Local::now();
    let candidate = now
        .date_naive()
        .and_time(time)
        .and_local_timezone(Local)
        .unwrap();
    if candidate > now {
        candidate
    } else {
        (now.date_naive() + Duration::days(1))
            .and_time(time)
            .and_local_timezone(Local)
            .unwrap()
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
    let today_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(Local)
        .unwrap()
        .to_rfc3339();
    let today_end = now
        .date_naive()
        .and_hms_opt(23, 59, 59)
        .unwrap()
        .and_local_timezone(Local)
        .unwrap()
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

/// Runs forever: sleeps until the configured morning-brief time, sends the brief, repeats.
pub async fn loop_forever(db: Arc<DbHandle>, am: AdapterManager) {
    loop {
        let brief_time = load_brief_time(&db).await;
        let next = next_occurrence(brief_time);
        let delay = (next - Local::now()).to_std().unwrap_or_default();

        info!(at = %next, "morning brief scheduled");
        tokio::time::sleep(delay).await;

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
