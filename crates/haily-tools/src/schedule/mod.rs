//! Deterministic VN/EN natural-language schedule parser for `reminder_add` — turns a phrase
//! like "nhắc tôi mỗi thứ 2" or "tomorrow 8am" into `(fire_at RFC3339, recurrence_rule)`.
//! Intentionally NOT an LLM call: offline, testable, and immune to prompt-injection via a
//! reminder title (Security Considerations, phase-02) — untrusted user text is only ever
//! matched against a small fixed phrase set, never executed or interpolated into SQL.
//!
//! Timezone: wall-clock phrases are interpreted via chrono `Local` (mirrors
//! `haily-proactive::morning_brief`/`dnd`) — no stored `user.timezone`, and VN has no DST, so
//! there is deliberately no `tz` parameter. `now` is passed in by the caller (never read from
//! the wall clock here) so parsing stays deterministic and testable.
mod tokens;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, Weekday};
use haily_db::recurrence::RecurrenceRule;

/// Bounds untrusted input length before any parsing work — a reminder title/phrase has no
/// legitimate reason to run into the kilobytes.
const MAX_INPUT_LEN: usize = 200;
const DEFAULT_HOUR: u32 = 8;

/// Parse a natural-language schedule phrase anchored at `now`. Returns `(fire_at, rule)` on
/// success — `rule` is `Some(canonical_rule_string)` for a recurring phrase (round-trips
/// through `RecurrenceRule::parse`/`to_canonical`, the SAME grammar `haily-db::recurrence`
/// consumes) or `None` for a one-shot phrase. Returns `None` for anything unrecognized —
/// callers must fall back to requiring an explicit RFC3339 `fire_at`, never guess.
pub fn parse_schedule(text: &str, now: DateTime<Local>) -> Option<(String, Option<String>)> {
    if text.is_empty() || text.chars().count() > MAX_INPUT_LEN {
        return None;
    }
    let lower = text.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let default_time = NaiveTime::from_hms_opt(DEFAULT_HOUR, 0, 0)?;

    let time = tokens::extract_time(&chars);
    let weekday = tokens::extract_weekday(&lower);
    let every_n = tokens::extract_every_n_days(&lower);

    let is_recurring_marker = lower.contains("mỗi") || lower.contains("every");
    let is_daily = lower.contains("hàng ngày")
        || lower.contains("mỗi ngày")
        || lower.contains("every day")
        || lower.contains("daily");
    let is_weekly_generic =
        !is_daily && (lower.contains("hàng tuần") || lower.contains("mỗi tuần") || lower.contains("weekly"));
    let is_next_week = lower.contains("tuần sau") || lower.contains("next week");
    let is_tomorrow = lower.contains("mai") || lower.contains("tomorrow");
    let is_today = lower.contains("hôm nay") || lower.contains("today");

    // "every N days" — reject N==0 (degenerate: `next_after` could never advance).
    if let Some(n) = every_n {
        if n == 0 {
            return None;
        }
        let first = next_time_at_or_after(now, time.unwrap_or(default_time))?;
        return Some((first.to_rfc3339(), Some(RecurrenceRule::EveryNDays(n).to_canonical())));
    }

    // A named weekday — recurring ("mỗi thứ 2" / "every Monday") or a one-shot future date.
    if let Some(wd) = weekday {
        let first = next_weekday_at(now, wd, time.unwrap_or(default_time), is_next_week)?;
        let rule = is_recurring_marker.then(|| RecurrenceRule::WeeklyOn(wd).to_canonical());
        return Some((first.to_rfc3339(), rule));
    }

    if is_daily {
        let first = next_time_at_or_after(now, time.unwrap_or(default_time))?;
        return Some((first.to_rfc3339(), Some(RecurrenceRule::Daily.to_canonical())));
    }

    if is_weekly_generic {
        let first = next_time_at_or_after(now, time.unwrap_or(default_time))?;
        return Some((first.to_rfc3339(), Some(RecurrenceRule::Weekly.to_canonical())));
    }

    // One-shot: an explicit "today"/"tomorrow" marker (with or without a time)...
    if is_tomorrow || is_today {
        let date = if is_tomorrow { now.date_naive() + Duration::days(1) } else { now.date_naive() };
        let candidate = local_at(date, time.unwrap_or(default_time))?;
        if candidate <= now {
            // e.g. "today 7am" already elapsed — refuse rather than silently guess a
            // different day.
            return None;
        }
        return Some((candidate.to_rfc3339(), None));
    }

    // ...or a bare time-of-day with no day marker: nearest future occurrence.
    if let Some(t) = time {
        let candidate = next_time_at_or_after(now, t)?;
        return Some((candidate.to_rfc3339(), None));
    }

    None
}

/// Resolve a local wall-clock instant, tolerating DST ambiguity/gaps exactly like
/// `morning_brief::local_at` — `.earliest()` picks a deterministic instant for an ambiguous
/// fall-back overlap and yields `None` for a spring-forward gap (VN has no DST in practice,
/// but the parser may run on a host whose `Local` zone does).
fn local_at(date: NaiveDate, time: NaiveTime) -> Option<DateTime<Local>> {
    date.and_time(time).and_local_timezone(Local).earliest()
}

/// Next occurrence of wall-clock `time`: today if it's still ahead of `now`, else tomorrow.
fn next_time_at_or_after(now: DateTime<Local>, time: NaiveTime) -> Option<DateTime<Local>> {
    let today = local_at(now.date_naive(), time)?;
    if today > now {
        Some(today)
    } else {
        local_at(now.date_naive() + Duration::days(1), time)
    }
}

/// Next date (>= today) whose weekday is `target`, at wall-clock `time`. When the immediate
/// candidate has already elapsed (today IS `target` but `time` already passed), or when
/// `push_next_week` is set (VN "tuần sau" / EN "next week"), skips a further 7 days past the
/// immediate candidate.
fn next_weekday_at(
    now: DateTime<Local>,
    target: Weekday,
    time: NaiveTime,
    push_next_week: bool,
) -> Option<DateTime<Local>> {
    let cur = i64::from(now.weekday().num_days_from_monday());
    let tgt = i64::from(target.num_days_from_monday());
    let mut days_ahead = tgt - cur;
    if days_ahead < 0 {
        days_ahead += 7;
    }

    let mut candidate_date = now.date_naive() + Duration::days(days_ahead);
    let mut candidate = local_at(candidate_date, time)?;
    if candidate <= now {
        candidate_date += Duration::days(7);
        candidate = local_at(candidate_date, time)?;
    }
    if push_next_week {
        candidate_date += Duration::days(7);
        candidate = local_at(candidate_date, time)?;
    }
    Some(candidate)
}
