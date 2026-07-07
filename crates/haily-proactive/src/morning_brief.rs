/// Daily morning brief — synthesizes calendar + tasks + reminders + floored memory
/// into one correlated summary (not four disjoint lists), sent once per day.
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveTime};
use haily_db::{
    queries::{
        calendar::{self, CalendarEvent},
        meta, reminders,
        reminders::Reminder,
        tasks::{self, Task},
    },
    DbHandle,
};
use haily_io::{AdapterManager, Notification};
use haily_kms::KmsHandle;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const DEFAULT_BRIEF_TIME: &str = "07:30";
/// Cap on floored-memory facts surfaced in the brief — keeps it scannable (phase spec:
/// "cap the count small (e.g. <=3)").
const MEMORY_RECALL_LIMIT: usize = 3;

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

/// True if `text` contains a `[[wikilink]]`-style reference. Detection only — the
/// referenced note is never resolved (an FTS/exact-title lookup would be fragile
/// against a renamed or deleted note); the presence of the syntax is itself the
/// flagged signal, matching the phase's "keep rules few and explainable" guidance.
fn has_wikilink(text: &str) -> bool {
    text.find("[[")
        .is_some_and(|start| text[start + 2..].contains("]]"))
}

/// A reminder is "deadline-linked" to a task when one title case-insensitively
/// contains the other — a deliberately simple, explainable "same deadline" signal.
/// No fuzzy/semantic matching: correlation stays deterministic, never an LLM judge.
fn reminder_linked_task<'a>(reminder: &Reminder, candidates: &[&'a Task]) -> Option<&'a Task> {
    let r = reminder.title.to_lowercase();
    candidates
        .iter()
        .find(|t| {
            let t_title = t.title.to_lowercase();
            r.contains(&t_title) || t_title.contains(&r)
        })
        .copied()
}

/// Build the "today's themes" query for floored memory recall from the titles of
/// everything already surfaced in the brief. `None` when there is nothing to recall
/// against — the brief never forces an unrelated memory block onto an empty day.
///
/// Deliberately an FTS5 OR-disjunction of individual (deduped, lowercased) words —
/// NOT a naive space-joined phrase. `search_fts`'s bareword tokens are ANDed by
/// default, so a phrase built from several unrelated titles (a calendar event, a
/// task, a reminder) would require one fact to contain EVERY word from EVERY item
/// to match at all, silently making recall return nothing on any day with more than
/// one item. The OR form recalls a fact touching ANY of today's themes instead.
fn build_theme_query(
    events: &[CalendarEvent],
    tasks: &[&Task],
    rems: &[Reminder],
) -> Option<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut terms: Vec<String> = Vec::new();
    for title in events
        .iter()
        .map(|e| e.title.as_str())
        .chain(tasks.iter().map(|t| t.title.as_str()))
        .chain(rems.iter().map(|r| r.title.as_str()))
    {
        for word in title.split_whitespace() {
            let w = word.to_lowercase();
            if seen.insert(w.clone()) {
                terms.push(w);
            }
        }
    }
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

/// Exposed `pub` (not just `pub(crate)`) so `tests/morning_brief_synthesis.rs` — an
/// external integration test, needed to exercise the real DB + `KmsHandle` recall
/// path end-to-end — can call it directly, mirroring `backup::loop_forever`'s
/// existing pub-entrypoint pattern for this crate.
pub async fn generate_brief(db: &DbHandle, kms: &KmsHandle) -> String {
    let now = Local::now();
    // Midnight and 23:59:59 can both land in a DST transition on rare dates; fall back
    // to `now` itself (still within today) rather than panicking or skipping the brief.
    let today_start = local_at(now.date_naive(), NaiveTime::from_hms_opt(0, 0, 0).unwrap())
        .unwrap_or(now)
        .to_rfc3339();
    let today_end = local_at(
        now.date_naive(),
        NaiveTime::from_hms_opt(23, 59, 59).unwrap(),
    )
    .unwrap_or(now)
    .to_rfc3339();
    let today_date = now.format("%Y-%m-%d").to_string();
    let now_rfc3339 = now.to_rfc3339();

    let events = match calendar::upcoming(db, &today_start, &today_end).await {
        Ok(v) => v,
        Err(e) => {
            warn!("brief: calendar query failed: {e:#}");
            Vec::new()
        }
    };

    let active_tasks = match tasks::active(db).await {
        Ok(v) => v,
        Err(e) => {
            warn!("brief: tasks query failed: {e:#}");
            Vec::new()
        }
    };

    let rems = match reminders::pending(db, &today_end).await {
        Ok(v) => v,
        Err(e) => {
            warn!("brief: reminders query failed: {e:#}");
            Vec::new()
        }
    };

    let due_today: Vec<&Task> = active_tasks
        .iter()
        .filter(|t| {
            t.due_at
                .as_deref()
                .map(|d| d.starts_with(&today_date))
                .unwrap_or(false)
        })
        .collect();
    let due_today_ids: HashSet<&str> = due_today.iter().map(|t| t.id.as_str()).collect();
    let overdue: Vec<&Task> = active_tasks
        .iter()
        .filter(|t| {
            t.due_at
                .as_deref()
                .map(|d| d < now_rfc3339.as_str())
                .unwrap_or(false)
        })
        .collect();
    let urgent_upcoming: Vec<&Task> = active_tasks
        .iter()
        .filter(|t| {
            matches!(t.priority.as_str(), "urgent" | "high")
                && !due_today_ids.contains(t.id.as_str())
        })
        .collect();

    let mut parts: Vec<String> = Vec::new();

    // Correlation 1: a task due today whose `calendar_event_id` FK names one of
    // today's events — the one unambiguous cross-source signal available (no
    // time-window heuristic needed; the link is already explicit in the schema).
    let task_by_event: HashMap<&str, &Task> = due_today
        .iter()
        .filter_map(|t| t.calendar_event_id.as_deref().map(|id| (id, *t)))
        .collect();

    if !events.is_empty() || !due_today.is_empty() {
        let mut block = "📅 **Hôm nay**\n".to_string();
        for e in &events {
            let start = e.start_at.get(11..16).unwrap_or(&e.start_at);
            block.push_str(&format!("  • {start} — {}\n", e.title));
            if let Some(loc) = &e.location {
                block.push_str(&format!("    📍 {loc}\n"));
            }
            if let Some(t) = task_by_event.get(e.id.as_str()) {
                block.push_str(&format!("    🔗 Task liên quan: {}\n", t.title));
            }
        }
        // Due-today tasks not already surfaced via a calendar link above.
        for t in due_today
            .iter()
            .filter(|t| !task_by_event.values().any(|linked| linked.id == t.id))
        {
            let marker = if matches!(t.priority.as_str(), "urgent" | "high") {
                "⚡"
            } else {
                "•"
            };
            block.push_str(&format!("  {marker} [{}] {}\n", t.priority, t.title));
        }
        parts.push(block);
    }
    // Deliberately no "nothing today" filler `else` branch here (unlike the
    // pre-synthesis version, which always pushed one on a successful-but-empty
    // query): a silent section is the norm now that the brief is several
    // independently-optional correlated blocks, not a gap to fill — and it lets a
    // totally empty day fall through to the single friendly greeting below instead
    // of one lonely "no calendar today" line.

    // Correlation 2: an overdue task whose description references a note via
    // `[[wikilink]]` syntax — flagged so the user knows there is supporting context.
    if !overdue.is_empty() {
        let mut block = format!("⚠️ **Quá hạn** ({} tasks)\n", overdue.len());
        for t in &overdue {
            let due = t.due_at.as_deref().and_then(|d| d.get(..10)).unwrap_or("?");
            let note_flag = if t.description.as_deref().map(has_wikilink).unwrap_or(false) {
                " 📝"
            } else {
                ""
            };
            block.push_str(&format!(
                "  • [{}] {} — hạn {due}{note_flag}\n",
                t.priority, t.title
            ));
        }
        parts.push(block);
    }

    if !urgent_upcoming.is_empty() {
        let mut block = format!("⚡ **Ưu tiên cao (sắp tới)** ({})\n", urgent_upcoming.len());
        for t in &urgent_upcoming {
            let due = t
                .due_at
                .as_deref()
                .and_then(|d| d.get(..10))
                .unwrap_or("chưa có hạn");
            block.push_str(&format!("  • [{}] {} — {due}\n", t.priority, t.title));
        }
        parts.push(block);
    }

    // Correlation 3: a reminder whose title names one of today's/overdue tasks — a
    // "reminder tied to a deadline" cross-source link.
    if !rems.is_empty() {
        let deadline_pool: Vec<&Task> = due_today
            .iter()
            .copied()
            .chain(overdue.iter().copied())
            .collect();
        let mut block = format!("⏰ **Nhắc nhở hôm nay** ({})\n", rems.len());
        for r in &rems {
            let time = r.fire_at.get(11..16).unwrap_or(&r.fire_at);
            let link = reminder_linked_task(r, &deadline_pool)
                .map(|t| format!(" 🔗 {}", t.title))
                .unwrap_or_default();
            block.push_str(&format!("  • {time} — {}{link}\n", r.title));
        }
        parts.push(block);
    }

    // Floored memory recall (Phase 1's `search_hybrid`): only above-floor facts are
    // ever returned — `ANN_DIST_MAX`/`BM25_CUTOFF` default to `None` (no forced
    // injection), so an empty result here means nothing cleared the bar, not a bug.
    if let Some(theme) = build_theme_query(&events, &due_today, &rems) {
        match kms.search_hybrid(&theme, MEMORY_RECALL_LIMIT).await {
            Ok(facts) if !facts.is_empty() => {
                let mut block = "🧠 **Bối cảnh liên quan**\n".to_string();
                for f in &facts {
                    block.push_str(&format!("  • {}\n", f.text));
                }
                parts.push(block);
            }
            Ok(_) => {}
            Err(e) => warn!("brief: memory recall failed: {e:#}"),
        }
    }

    if parts.is_empty() {
        return format!(
            "Chào buổi sáng! 🌅 Hôm nay {} không có gì đặc biệt. Chúc ngày tốt lành.",
            now.format("%d/%m/%Y")
        );
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
pub async fn loop_forever(
    db: Arc<DbHandle>,
    kms: Arc<KmsHandle>,
    am: AdapterManager,
    shutdown: CancellationToken,
) {
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

        let brief = generate_brief(&db, &kms).await;
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

    #[test]
    fn has_wikilink_detects_the_bracket_pair_only() {
        assert!(has_wikilink("see [[Project Notes]] for context"));
        assert!(!has_wikilink("no reference here"));
        assert!(!has_wikilink("unterminated [[dangling"));
    }
}
