/// Do-not-disturb: check whether the current local time falls in the user's DND window.
///
/// Preferences:
///   `dnd.start` = "22:00"  (24h HH:MM local time)
///   `dnd.end`   = "07:00"  (24h HH:MM local time; may be next-day)
///
/// DND is disabled by default (if preferences are not set).
use chrono::{Local, NaiveTime};
use haily_db::{queries::meta, DbHandle};

fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    let mut parts = s.splitn(2, ':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    NaiveTime::from_hms_opt(h, m, 0)
}

/// Returns `true` if the current local time is inside the DND window.
pub async fn is_active(db: &DbHandle) -> bool {
    let start_str = match meta::get_preference(db, "dnd.start").await {
        Ok(Some(v)) => v,
        _ => return false, // no DND configured
    };
    let end_str = match meta::get_preference(db, "dnd.end").await {
        Ok(Some(v)) => v,
        _ => return false,
    };

    let (Some(dnd_start), Some(dnd_end)) = (parse_hhmm(&start_str), parse_hhmm(&end_str)) else {
        return false;
    };

    let now = Local::now().time();

    if dnd_start <= dnd_end {
        // Simple case: DND within the same calendar day (e.g., 02:00–06:00)
        now >= dnd_start && now < dnd_end
    } else {
        // Wraps midnight (e.g., 22:00–07:00)
        now >= dnd_start || now < dnd_end
    }
}
