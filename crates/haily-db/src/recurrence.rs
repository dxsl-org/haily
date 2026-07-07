//! Recurrence rule engine — the single, deterministic implementation of "what's the next
//! occurrence of this reminder," shared by every consumer that needs it (the proactive
//! daemon's fire loop today; the calendar recurrence phases 13a/13b reuse this SAME type
//! rather than forking, per phase-02's crate-home decision). Lives in `haily-db` because it
//! is the lowest common ancestor of `haily-proactive` (fires reminders) and `haily-tools`
//! (writes them) — `haily-proactive` depends on `{haily-db, haily-io}` only.
//!
//! Rules are stored as a small, closed string grammar in the existing `reminders.recurrence`
//! TEXT column (no migration needed): `daily`, `weekly`, `weekly:<mon..sun>`,
//! `monthly:<1..31>`, `every:<N>d`. Never build SQL from these strings — they are matched
//! against a fixed enum via `parse`, never interpolated.
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, TimeZone, Utc, Weekday};

/// Hard ceiling on how many single-period steps `next_after` will take while coalescing a
/// backlog of missed occurrences into one. Every valid rule (post-`parse` validation) always
/// advances by at least one calendar day per step, so reaching this bound would mean `base`
/// is implausibly stale — a defense-in-depth cap, not an expected code path.
const MAX_COALESCE_STEPS: u32 = 10_000;

/// Cap on how many in-window occurrences `occurrences_in_window` will collect for a single
/// event expansion — 366 comfortably covers a full year of a daily event, far more than any
/// realistic `upcoming` query window, while bounding worst-case memory/CPU per event.
const MAX_OCCURRENCES_PER_EVENT: usize = 366;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecurrenceRule {
    Daily,
    Weekly,
    WeeklyOn(Weekday),
    Monthly(u32),
    EveryNDays(u32),
}

impl RecurrenceRule {
    /// Parse the canonical rule grammar. Rejects `every:0d`/non-numeric `every:`, any
    /// `monthly:<day>` outside `1..=31`, and any unrecognized `weekly:<code>` — a degenerate
    /// rule would otherwise make `next_after` stall (a zero-length step) or the caller would
    /// silently store garbage that can never fire correctly.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s == "daily" {
            return Some(Self::Daily);
        }
        if s == "weekly" {
            return Some(Self::Weekly);
        }
        if let Some(rest) = s.strip_prefix("weekly:") {
            return weekday_from_code(rest).map(Self::WeeklyOn);
        }
        if let Some(rest) = s.strip_prefix("monthly:") {
            let day: u32 = rest.parse().ok()?;
            return if (1..=31).contains(&day) { Some(Self::Monthly(day)) } else { None };
        }
        if let Some(rest) = s.strip_prefix("every:") {
            let digits = rest.strip_suffix('d')?;
            let n: u32 = digits.parse().ok()?;
            return if n >= 1 { Some(Self::EveryNDays(n)) } else { None };
        }
        None
    }

    /// Canonical string form — the inverse of `parse`. Used by the NL parser (`haily-tools`)
    /// so the stored grammar has exactly one producer/consumer pair, never a hand-duplicated
    /// format string on the writer side.
    pub fn to_canonical(&self) -> String {
        match self {
            Self::Daily => "daily".to_string(),
            Self::Weekly => "weekly".to_string(),
            Self::WeeklyOn(wd) => format!("weekly:{}", weekday_to_code(*wd)),
            Self::Monthly(d) => format!("monthly:{d}"),
            Self::EveryNDays(n) => format!("every:{n}d"),
        }
    }

    /// Advance `from` by exactly one period of this rule, preserving `from`'s time-of-day and
    /// UTC offset. Every arm advances by >= 1 calendar day — the invariant `next_after`'s
    /// coalesce loop relies on to terminate.
    fn step(&self, from: DateTime<FixedOffset>) -> Option<DateTime<FixedOffset>> {
        match self {
            Self::Daily => Some(from + Duration::days(1)),
            Self::Weekly => Some(from + Duration::days(7)),
            Self::EveryNDays(n) => Some(from + Duration::days(i64::from(*n))),
            Self::WeeklyOn(target) => {
                let cur = i64::from(from.weekday().num_days_from_monday());
                let tgt = i64::from(target.num_days_from_monday());
                let mut delta = tgt - cur;
                if delta <= 0 {
                    delta += 7;
                }
                Some(from + Duration::days(delta))
            }
            Self::Monthly(day) => add_one_month_clamped(from, *day),
        }
    }

    /// First occurrence strictly AFTER `now`, computed from `base` (the LATEST known
    /// occurrence — i.e. the `fire_at` of the reminder that just fired) by stepping forward
    /// one period at a time until the candidate clears `now`. This coalesces an arbitrary
    /// backlog of missed periods (daemon asleep for a week) into exactly ONE resulting
    /// occurrence, rather than reconstructing every missed tick — the contract
    /// `haily-proactive::reminders` depends on to avoid a drip-storm of one-fire-per-poll
    /// on wake.
    pub fn next_after(&self, base: &str, now: DateTime<Utc>) -> Result<String> {
        let base_dt = DateTime::parse_from_rfc3339(base)
            .map_err(|e| anyhow!("recurrence next_after: invalid base fire_at '{base}': {e}"))?;

        let mut candidate = self.step(base_dt).ok_or_else(|| {
            anyhow!("recurrence next_after: step from '{base}' produced no valid date")
        })?;
        let mut steps = 0u32;
        while candidate <= now {
            candidate = self.step(candidate).ok_or_else(|| {
                anyhow!("recurrence next_after: step from '{candidate}' produced no valid date")
            })?;
            steps += 1;
            if steps > MAX_COALESCE_STEPS {
                return Err(anyhow!(
                    "recurrence next_after: exceeded {MAX_COALESCE_STEPS} coalesce steps from base '{base}'"
                ));
            }
        }
        Ok(candidate.to_rfc3339())
    }

    /// Enumerate every occurrence of this rule, seeded from `base` (the event's stored
    /// `start_at`), that falls within `[window_from, window_to]` inclusive — the expansion
    /// primitive `haily-db::queries::calendar::upcoming` uses (phase 13a) so a recurring
    /// calendar event surfaces on every in-window occurrence instead of only its stored
    /// `start_at`. Reuses `step` (the same period-advance `next_after` relies on) rather than
    /// forking any recurrence math.
    ///
    /// Two bounds keep this safe against a long-lived event combined with a huge caller
    /// window: fast-forwarding from `base` to `window_from` is capped at
    /// `MAX_COALESCE_STEPS` (mirrors `next_after`'s own backlog-coalescing bound), and the
    /// number of occurrences collected inside the window is capped at
    /// `MAX_OCCURRENCES_PER_EVENT` — a defense-in-depth guard against a pathological rule or
    /// window exhausting memory (Security Considerations, phase 13a).
    pub fn occurrences_in_window(
        &self,
        base: DateTime<FixedOffset>,
        window_from: DateTime<FixedOffset>,
        window_to: DateTime<FixedOffset>,
    ) -> Vec<DateTime<FixedOffset>> {
        if window_from > window_to || base > window_to {
            return Vec::new();
        }

        let mut candidate = base;
        let mut coalesce_steps = 0u32;
        while candidate < window_from {
            candidate = match self.step(candidate) {
                Some(c) => c,
                None => return Vec::new(),
            };
            coalesce_steps += 1;
            if coalesce_steps > MAX_COALESCE_STEPS {
                return Vec::new();
            }
        }

        let mut occurrences = Vec::new();
        while candidate <= window_to && occurrences.len() < MAX_OCCURRENCES_PER_EVENT {
            occurrences.push(candidate);
            candidate = match self.step(candidate) {
                Some(c) => c,
                None => break,
            };
        }
        occurrences
    }
}

/// Convenience wrapper for callers holding the rule as a raw stored string (the daemon's fire
/// path) — parses then delegates to `RecurrenceRule::next_after`. Returns `Ok(None)` for an
/// unparseable rule (should not occur for a rule validated at write time; the caller treats
/// `None` as "cannot reschedule, fire as one-shot" rather than panicking on stored garbage).
pub fn next_after(rule: &str, base: &str, now: DateTime<Utc>) -> Result<Option<String>> {
    match RecurrenceRule::parse(rule) {
        Some(r) => r.next_after(base, now).map(Some),
        None => Ok(None),
    }
}

fn weekday_from_code(code: &str) -> Option<Weekday> {
    match code {
        "mon" => Some(Weekday::Mon),
        "tue" => Some(Weekday::Tue),
        "wed" => Some(Weekday::Wed),
        "thu" => Some(Weekday::Thu),
        "fri" => Some(Weekday::Fri),
        "sat" => Some(Weekday::Sat),
        "sun" => Some(Weekday::Sun),
        _ => None,
    }
}

fn weekday_to_code(wd: Weekday) -> &'static str {
    match wd {
        Weekday::Mon => "mon",
        Weekday::Tue => "tue",
        Weekday::Wed => "wed",
        Weekday::Thu => "thu",
        Weekday::Fri => "fri",
        Weekday::Sat => "sat",
        Weekday::Sun => "sun",
    }
}

/// Last valid day-of-month for `year`/`month`, via "first day of next month, minus one day" —
/// avoids a hand-rolled leap-year table. `None` only if the underlying date arithmetic itself
/// is out of `NaiveDate`'s representable range.
fn last_day_of_month(year: i32, month: u32) -> Option<u32> {
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let first_of_next = NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    Some(first_of_next.pred_opt()?.day())
}

/// Step `from` forward exactly one calendar month, clamping `day` to the target month's last
/// day (e.g. `monthly:31` in February lands on the 28th/29th) — never rolls into the
/// FOLLOWING month, which a naive `+ Duration::days(31)` would risk.
fn add_one_month_clamped(from: DateTime<FixedOffset>, day: u32) -> Option<DateTime<FixedOffset>> {
    let (year, month) =
        if from.month() == 12 { (from.year() + 1, 1) } else { (from.year(), from.month() + 1) };
    let clamped_day = day.min(last_day_of_month(year, month)?);
    let naive_date = NaiveDate::from_ymd_opt(year, month, clamped_day)?;
    let naive_dt = naive_date.and_time(from.time());
    from.timezone().from_local_datetime(&naive_dt).single()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn parse_rejects_degenerate_rules() {
        assert!(RecurrenceRule::parse("every:0d").is_none());
        assert!(RecurrenceRule::parse("every:0").is_none(), "missing 'd' suffix");
        assert!(RecurrenceRule::parse("every:-1d").is_none());
        assert!(RecurrenceRule::parse("every:notanumber").is_none());
        assert!(RecurrenceRule::parse("monthly:0").is_none());
        assert!(RecurrenceRule::parse("monthly:32").is_none());
        assert!(RecurrenceRule::parse("weekly:funday").is_none());
        assert!(RecurrenceRule::parse("cron: * * * * *").is_none());
        assert!(RecurrenceRule::parse("garbage").is_none());
    }

    #[test]
    fn parse_accepts_every_supported_shape() {
        assert_eq!(RecurrenceRule::parse("daily"), Some(RecurrenceRule::Daily));
        assert_eq!(RecurrenceRule::parse("weekly"), Some(RecurrenceRule::Weekly));
        assert_eq!(RecurrenceRule::parse("weekly:mon"), Some(RecurrenceRule::WeeklyOn(Weekday::Mon)));
        assert_eq!(RecurrenceRule::parse("monthly:31"), Some(RecurrenceRule::Monthly(31)));
        assert_eq!(RecurrenceRule::parse("every:3d"), Some(RecurrenceRule::EveryNDays(3)));
    }

    #[test]
    fn to_canonical_round_trips_through_parse() {
        for rule in [
            RecurrenceRule::Daily,
            RecurrenceRule::Weekly,
            RecurrenceRule::WeeklyOn(Weekday::Fri),
            RecurrenceRule::Monthly(15),
            RecurrenceRule::EveryNDays(4),
        ] {
            assert_eq!(RecurrenceRule::parse(&rule.to_canonical()), Some(rule));
        }
    }

    /// CRITICAL forward-progress contract: a recurring reminder whose daemon was asleep for a
    /// week must fire ONCE on wake, with the next `fire_at` strictly in the future — not a
    /// drip-storm reconstructing every missed daily tick.
    #[test]
    fn daily_reminder_asleep_a_week_coalesces_to_one_future_occurrence() {
        let base = "2026-01-01T07:00:00+00:00";
        let now = dt("2026-01-08T20:00:00+00:00"); // a week + 13h later
        let next = RecurrenceRule::Daily.next_after(base, now).unwrap();
        let next_dt = DateTime::parse_from_rfc3339(&next).unwrap();
        assert!(next_dt.with_timezone(&Utc) > now, "next_after must be strictly after now");
        assert_eq!(next, "2026-01-09T07:00:00+00:00");
    }

    #[test]
    fn weekday_rollover_advances_a_full_week_when_target_equals_current_day() {
        // 2026-01-05 is a Monday.
        let rule = RecurrenceRule::WeeklyOn(Weekday::Mon);
        let base = "2026-01-05T08:00:00+00:00";
        let now = dt("2026-01-05T09:00:00+00:00");
        let next = rule.next_after(base, now).unwrap();
        assert_eq!(next, "2026-01-12T08:00:00+00:00");
    }

    #[test]
    fn weekday_rollover_lands_on_the_immediate_upcoming_target_day() {
        // 2026-01-05 is a Monday; next Friday is 2026-01-09.
        let rule = RecurrenceRule::WeeklyOn(Weekday::Fri);
        let base = "2026-01-05T08:00:00+00:00";
        let now = dt("2026-01-05T09:00:00+00:00");
        let next = rule.next_after(base, now).unwrap();
        assert_eq!(next, "2026-01-09T08:00:00+00:00");
    }

    #[test]
    fn monthly_clamps_to_the_last_day_of_a_shorter_month() {
        let rule = RecurrenceRule::Monthly(31);
        let base = "2026-01-31T08:00:00+00:00";
        let now = dt("2026-01-31T09:00:00+00:00");
        let next = rule.next_after(base, now).unwrap();
        assert_eq!(next, "2026-02-28T08:00:00+00:00"); // 2026 is not a leap year
    }

    #[test]
    fn every_n_days_steps_by_n() {
        let rule = RecurrenceRule::EveryNDays(3);
        let base = "2026-03-01T08:00:00+00:00";
        let now = dt("2026-03-01T09:00:00+00:00");
        let next = rule.next_after(base, now).unwrap();
        assert_eq!(next, "2026-03-04T08:00:00+00:00");
    }

    #[test]
    fn next_after_rejects_an_unparseable_base_timestamp() {
        assert!(RecurrenceRule::Daily.next_after("not-a-date", Utc::now()).is_err());
    }

    #[test]
    fn free_function_returns_none_for_an_unparseable_stored_rule() {
        let result = next_after("garbage-rule", "2026-01-01T00:00:00+00:00", Utc::now()).unwrap();
        assert!(result.is_none());
    }

    fn fixed(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    #[test]
    fn occurrences_in_window_yields_one_per_period_across_the_range() {
        // Weekly-on-Monday event that started 3 weeks before the query window; the window
        // spans 4 Mondays — every one of them must surface.
        let rule = RecurrenceRule::WeeklyOn(Weekday::Mon);
        let base = fixed("2025-12-15T08:00:00+00:00"); // a Monday
        let from = fixed("2026-01-05T00:00:00+00:00");
        let to = fixed("2026-01-27T23:59:59+00:00");
        let occurrences = rule.occurrences_in_window(base, from, to);
        let rendered: Vec<String> = occurrences.iter().map(|d| d.to_rfc3339()).collect();
        assert_eq!(
            rendered,
            vec![
                "2026-01-05T08:00:00+00:00",
                "2026-01-12T08:00:00+00:00",
                "2026-01-19T08:00:00+00:00",
                "2026-01-26T08:00:00+00:00",
            ]
        );
    }

    #[test]
    fn occurrences_in_window_includes_base_when_base_is_inside_the_window() {
        let rule = RecurrenceRule::Daily;
        let base = fixed("2026-02-01T09:00:00+00:00");
        let from = fixed("2026-02-01T00:00:00+00:00");
        let to = fixed("2026-02-03T23:59:59+00:00");
        let occurrences = rule.occurrences_in_window(base, from, to);
        assert_eq!(occurrences.len(), 3);
        assert_eq!(occurrences[0], base);
    }

    #[test]
    fn occurrences_in_window_is_empty_when_base_is_after_the_window() {
        let rule = RecurrenceRule::Daily;
        let base = fixed("2026-05-01T09:00:00+00:00");
        let from = fixed("2026-02-01T00:00:00+00:00");
        let to = fixed("2026-02-03T23:59:59+00:00");
        assert!(rule.occurrences_in_window(base, from, to).is_empty());
    }

    #[test]
    fn occurrences_in_window_is_bounded_by_the_per_event_cap() {
        // A daily rule over a ~2-year window would otherwise yield ~730 occurrences; the cap
        // must bound the result regardless of how wide the caller's window is.
        let rule = RecurrenceRule::Daily;
        let base = fixed("2024-01-01T08:00:00+00:00");
        let from = fixed("2024-01-01T00:00:00+00:00");
        let to = fixed("2026-01-01T00:00:00+00:00");
        let occurrences = rule.occurrences_in_window(base, from, to);
        assert_eq!(occurrences.len(), MAX_OCCURRENCES_PER_EVENT);
    }
}
