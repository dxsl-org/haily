//! Token-level extraction for `parse_schedule` — weekday names, "every N days" counts, and
//! 12/24-hour time-of-day, for both VN and EN phrasing. Pure functions, no I/O; every
//! extractor takes the already-lowercased input. Time extraction walks a `Vec<char>` (not
//! byte-indexed `&str` slicing) so multi-byte Vietnamese diacritics are never split mid-char.
use chrono::{NaiveTime, Weekday};

/// VN + EN weekday phrases mapped to `chrono::Weekday`, checked via substring containment.
/// None of these phrases are substrings of one another, so match order does not matter.
const WEEKDAY_PHRASES: &[(&str, Weekday)] = &[
    ("chủ nhật", Weekday::Sun),
    ("thứ hai", Weekday::Mon),
    ("thứ ba", Weekday::Tue),
    ("thứ tư", Weekday::Wed),
    ("thứ năm", Weekday::Thu),
    ("thứ sáu", Weekday::Fri),
    ("thứ bảy", Weekday::Sat),
    ("thứ 2", Weekday::Mon),
    ("thứ 3", Weekday::Tue),
    ("thứ 4", Weekday::Wed),
    ("thứ 5", Weekday::Thu),
    ("thứ 6", Weekday::Fri),
    ("thứ 7", Weekday::Sat),
    ("monday", Weekday::Mon),
    ("tuesday", Weekday::Tue),
    ("wednesday", Weekday::Wed),
    ("thursday", Weekday::Thu),
    ("friday", Weekday::Fri),
    ("saturday", Weekday::Sat),
    ("sunday", Weekday::Sun),
];

/// First weekday phrase found in `text` (already lowercased). `None` if no phrase matches.
pub fn extract_weekday(text: &str) -> Option<Weekday> {
    WEEKDAY_PHRASES.iter().find(|(p, _)| text.contains(p)).map(|(_, wd)| *wd)
}

/// `"mỗi N ngày"` / `"every N days"` → N. Returns `Some(0)` for an explicit zero (rather than
/// `None`) so the caller can reject the degenerate count explicitly instead of it silently
/// falling through to a different rule shape.
pub fn extract_every_n_days(text: &str) -> Option<u32> {
    let (idx, marker_len) = if let Some(i) = text.find("mỗi ") {
        (i, "mỗi ".len())
    } else if let Some(i) = text.find("every ") {
        (i, "every ".len())
    } else {
        return None;
    };
    let rest = &text[idx + marker_len..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let after = rest[digits.len()..].trim_start();
    if after.starts_with("ngày") || after.starts_with("day") {
        digits.parse().ok()
    } else {
        None
    }
}

/// Extract a wall-clock time-of-day from VN (`"7h"`, `"7h30"`) or EN (`"8am"`, `"8:30pm"`)
/// phrasing. `chars` must already be lowercased.
pub fn extract_time(chars: &[char]) -> Option<NaiveTime> {
    extract_time_vn(chars).or_else(|| extract_time_en(chars))
}

/// VN hour digits are taken literally (24h) unless followed by an explicit "chiều"/"tối"
/// (afternoon/evening) marker, which adds 12 to an hour < 12 — a bare "sáng" (morning) marker
/// is recognized but never shifts the hour, since a bare VN hour is already conventionally
/// spoken in the "sáng" sense by default (e.g. "7h" alone means 7 AM, not 7 PM).
fn extract_time_vn(chars: &[char]) -> Option<NaiveTime> {
    // Find an 'h' immediately preceded by a digit — distinguishes the time marker "7h" from
    // an incidental 'h' inside a VN word like "hàng".
    let h_pos = (1..chars.len()).find(|&i| chars[i] == 'h' && chars[i - 1].is_ascii_digit())?;

    let mut start = h_pos;
    while start > 0 && chars[start - 1].is_ascii_digit() {
        start -= 1;
    }
    let mut hour: u32 = chars[start..h_pos].iter().collect::<String>().parse().ok()?;
    if hour > 23 {
        return None;
    }

    let mut end = h_pos + 1;
    while end < chars.len() && chars[end].is_ascii_digit() {
        end += 1;
    }
    let minute: u32 = if end == h_pos + 1 {
        0
    } else {
        chars[h_pos + 1..end].iter().collect::<String>().parse().ok()?
    };
    if minute > 59 {
        return None;
    }

    let tail: String = chars[end..].iter().collect();
    if hour < 12 && (tail.contains("chiều") || tail.contains("tối")) {
        hour += 12;
    }
    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// EN 12-hour `H(:MM)?am|pm` phrasing (e.g. "8am", "8:30pm"). Scans for the literal `am`/`pm`
/// marker then walks backward over digits/colon to recover the numeric token.
fn extract_time_en(chars: &[char]) -> Option<NaiveTime> {
    let (marker_pos, is_pm) = (0..chars.len().saturating_sub(1)).find_map(|i| {
        if chars[i] == 'a' && chars[i + 1] == 'm' {
            Some((i, false))
        } else if chars[i] == 'p' && chars[i + 1] == 'm' {
            Some((i, true))
        } else {
            None
        }
    })?;

    let mut end = marker_pos;
    while end > 0 && chars[end - 1] == ' ' {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && (chars[start - 1].is_ascii_digit() || chars[start - 1] == ':') {
        start -= 1;
    }
    if start == end {
        return None;
    }

    let token: String = chars[start..end].iter().collect();
    let mut parts = token.splitn(2, ':');
    let mut hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = match parts.next() {
        Some(m) if !m.is_empty() => m.parse().ok()?,
        _ => 0,
    };
    if !(1..=12).contains(&hour) || minute > 59 {
        return None;
    }
    if is_pm && hour != 12 {
        hour += 12;
    } else if !is_pm && hour == 12 {
        hour = 0;
    }
    NaiveTime::from_hms_opt(hour, minute, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_weekday_matches_vn_and_en_phrases() {
        assert_eq!(extract_weekday("nhắc tôi mỗi thứ 2"), Some(Weekday::Mon));
        assert_eq!(extract_weekday("every monday"), Some(Weekday::Mon));
        assert_eq!(extract_weekday("chủ nhật này"), Some(Weekday::Sun));
        assert_eq!(extract_weekday("no weekday here"), None);
    }

    #[test]
    fn extract_every_n_days_reads_the_count_and_rejects_non_matches() {
        assert_eq!(extract_every_n_days("mỗi 3 ngày"), Some(3));
        assert_eq!(extract_every_n_days("every 5 days"), Some(5));
        assert_eq!(extract_every_n_days("mỗi 0 ngày"), Some(0));
        assert_eq!(extract_every_n_days("mỗi thứ 2"), None);
        assert_eq!(extract_every_n_days("hàng ngày"), None);
    }

    #[test]
    fn extract_time_reads_vn_hour_and_minute() {
        let chars: Vec<char> = "hàng ngày 7h".chars().collect();
        assert_eq!(extract_time(&chars), NaiveTime::from_hms_opt(7, 0, 0));

        let chars: Vec<char> = "7h30 sáng mai".chars().collect();
        assert_eq!(extract_time(&chars), NaiveTime::from_hms_opt(7, 30, 0));

        let chars: Vec<char> = "7h tối nay".chars().collect();
        assert_eq!(extract_time(&chars), NaiveTime::from_hms_opt(19, 0, 0));
    }

    #[test]
    fn extract_time_reads_en_12_hour_clock() {
        let chars: Vec<char> = "tomorrow 8am".chars().collect();
        assert_eq!(extract_time(&chars), NaiveTime::from_hms_opt(8, 0, 0));

        let chars: Vec<char> = "8:30pm".chars().collect();
        assert_eq!(extract_time(&chars), NaiveTime::from_hms_opt(20, 30, 0));

        let chars: Vec<char> = "12am".chars().collect();
        assert_eq!(extract_time(&chars), NaiveTime::from_hms_opt(0, 0, 0));
    }

    #[test]
    fn extract_time_returns_none_when_absent() {
        let chars: Vec<char> = "no time phrase here".chars().collect();
        assert_eq!(extract_time(&chars), None);
    }
}
