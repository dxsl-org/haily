//! VN + EN natural-language schedule parsing tests (phase-02) — deterministic, no network,
//! anchored to an explicit `now` so results never depend on the host's wall clock or run
//! date. `Local` here is whatever timezone the test host runs in — the design intentionally
//! has no `tz` parameter (no `user.timezone` is stored, VN has no DST), so every assertion is
//! built the same way the parser itself resolves wall-clock time, not hardcoded to a specific
//! UTC offset.
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone, Utc};
use haily_tools::schedule::parse_schedule;

/// A fixed anchor for every test: 2026-07-06 (Monday) 09:00 local.
fn anchor() -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 7, 6, 9, 0, 0).single().expect("anchor is an unambiguous local time")
}

#[test]
fn vn_weekly_recurring_weekday_phrase_derives_weekly_mon() {
    let (fire_at, rule) = parse_schedule("nhắc tôi mỗi thứ 2", anchor()).expect("should parse");
    assert_eq!(rule.as_deref(), Some("weekly:mon"));

    let parsed = DateTime::parse_from_rfc3339(&fire_at).expect("valid rfc3339");
    assert_eq!(parsed.weekday(), chrono::Weekday::Mon);
    assert!(parsed > anchor(), "derived fire_at must be in the future");
}

#[test]
fn vn_daily_with_explicit_time_derives_daily_at_seven() {
    let (fire_at, rule) = parse_schedule("hàng ngày 7h", anchor()).expect("should parse");
    assert_eq!(rule.as_deref(), Some("daily"));

    let parsed = DateTime::parse_from_rfc3339(&fire_at).expect("valid rfc3339");
    assert_eq!(parsed.naive_local().time(), NaiveTime::from_hms_opt(7, 0, 0).unwrap());
}

#[test]
fn en_one_shot_tomorrow_with_time_derives_no_rule() {
    let (fire_at, rule) = parse_schedule("tomorrow 8am", anchor()).expect("should parse");
    assert!(rule.is_none(), "a one-shot phrase must not derive a recurrence rule");

    let parsed = DateTime::parse_from_rfc3339(&fire_at).expect("valid rfc3339");
    let expected_date = anchor().date_naive() + Duration::days(1);
    assert_eq!(parsed.date_naive(), expected_date);
    assert_eq!(parsed.naive_local().time(), NaiveTime::from_hms_opt(8, 0, 0).unwrap());
}

/// Locked timezone contract (no `tz` param, no DST): a VN wall-clock phrase must map to the
/// SAME UTC instant as manually constructing "tomorrow 07:00" via `chrono::Local` — proving
/// the offset baked into the returned RFC3339 string is correct for whatever timezone the
/// host runs in, not hardcoded to a specific offset.
#[test]
fn vn_local_wall_clock_phrase_maps_to_the_correct_utc_instant() {
    let (fire_at, rule) = parse_schedule("7h sáng mai", anchor()).expect("should parse");
    assert!(rule.is_none());

    let actual = DateTime::parse_from_rfc3339(&fire_at)
        .expect("valid rfc3339")
        .with_timezone(&Utc);

    let expected_date = anchor().date_naive() + Duration::days(1);
    let expected_local = Local
        .from_local_datetime(&expected_date.and_time(NaiveTime::from_hms_opt(7, 0, 0).unwrap()))
        .single()
        .expect("unambiguous local time");
    let expected = expected_local.with_timezone(&Utc);

    assert_eq!(actual, expected);
}

#[test]
fn unparseable_phrase_returns_none() {
    assert!(parse_schedule("asdlkfj qqq random text", anchor()).is_none());
}

/// `parse_schedule` must reject a degenerate "every N days" count — N<=0 can never advance,
/// which would otherwise make the stored rule stall or re-fire every poll.
#[test]
fn degenerate_every_zero_days_is_rejected() {
    assert!(parse_schedule("mỗi 0 ngày", anchor()).is_none());
    assert!(parse_schedule("every 0 days", anchor()).is_none());
}

#[test]
fn recurring_every_n_days_phrase_derives_the_canonical_rule() {
    let (_, rule) = parse_schedule("mỗi 3 ngày", anchor()).expect("should parse");
    assert_eq!(rule.as_deref(), Some("every:3d"));
}

#[test]
fn input_length_is_bounded() {
    let long_input = "a".repeat(500);
    assert!(parse_schedule(&long_input, anchor()).is_none());
}

#[test]
fn empty_input_is_rejected() {
    assert!(parse_schedule("", anchor()).is_none());
}
