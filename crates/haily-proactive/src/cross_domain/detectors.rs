/// Pure cross-domain detector functions — data in, `NudgeCandidate`s out. Deliberately
/// free of DB/IO so they're unit-testable without a database or the event loop (see
/// `tests/cross_domain_nudges.rs`).
///
/// Each event/task falls into exactly one condition per detector (the cross-domain-
/// specific one when it applies, else the plain baseline) — mutually exclusive so a
/// single entity never produces two overlapping alerts on the same tick.
use haily_db::queries::{calendar::CalendarEvent, tasks::Task};

pub const COND_MEETING_IMMINENT: &str = "meeting_imminent";
pub const COND_MEETING_PREP_INCOMPLETE: &str = "meeting_prep_incomplete";
pub const COND_EVENT_NO_PREP: &str = "event_no_prep_task";
pub const COND_TASK_OVERDUE: &str = "task_overdue";
pub const COND_OVERDUE_BLOCKS_MEETING: &str = "overdue_blocks_meeting";

/// A detected condition, pending cooldown/DND filtering by the caller.
pub struct NudgeCandidate {
    pub condition: &'static str,
    pub entity_id: String,
    pub title: String,
    pub body: String,
    pub urgent: bool,
}

/// Per imminent event: an active (not done/cancelled) linked prep task means the user is
/// about to walk into the meeting with unfinished prep (`COND_MEETING_PREP_INCOMPLETE`);
/// otherwise falls back to the plain "meeting soon" reminder (`COND_MEETING_IMMINENT`,
/// preserving the pre-existing baseline behavior this phase broadens).
pub fn detect_meeting_conditions(
    imminent_events: &[CalendarEvent],
    active_tasks: &[Task],
) -> Vec<NudgeCandidate> {
    imminent_events
        .iter()
        .map(|e| {
            let start = e.start_at.get(11..16).unwrap_or(&e.start_at);
            match active_tasks.iter().find(|t| t.calendar_event_id.as_deref() == Some(e.id.as_str())) {
                Some(t) => NudgeCandidate {
                    condition: COND_MEETING_PREP_INCOMPLETE,
                    entity_id: e.id.clone(),
                    title: "⏰ Họp sắp tới, task chuẩn bị chưa xong".to_string(),
                    body: format!("{} lúc {start} — task '{}' chưa hoàn thành", e.title, t.title),
                    urgent: true,
                },
                None => NudgeCandidate {
                    condition: COND_MEETING_IMMINENT,
                    entity_id: e.id.clone(),
                    title: "📅 Sắp có cuộc họp".to_string(),
                    body: format!("{} lúc {start}", e.title),
                    urgent: false,
                },
            }
        })
        .collect()
}

/// Per event happening today: no task at all (any status) links to it via
/// `calendar_event_id`. `linked_tasks` must include every status — a *done* prep task
/// must NOT be reported as "no prep task", which is why the caller passes
/// `tasks::linked_to_calendar` (all statuses) rather than `tasks::active`.
pub fn detect_event_no_prep(
    today_events: &[CalendarEvent],
    linked_tasks: &[Task],
) -> Vec<NudgeCandidate> {
    today_events
        .iter()
        .filter(|e| !linked_tasks.iter().any(|t| t.calendar_event_id.as_deref() == Some(e.id.as_str())))
        .map(|e| {
            let start = e.start_at.get(11..16).unwrap_or(&e.start_at);
            NudgeCandidate {
                condition: COND_EVENT_NO_PREP,
                entity_id: e.id.clone(),
                title: "📋 Sự kiện chưa có task chuẩn bị".to_string(),
                body: format!("{} lúc {start} hôm nay chưa có task chuẩn bị nào", e.title),
                urgent: false,
            }
        })
        .collect()
}

/// Per overdue task: a `calendar_event_id` link to a not-yet-started event within the
/// caller's commitment horizon means the missed deadline threatens a real calendared
/// commitment (`COND_OVERDUE_BLOCKS_MEETING`); otherwise falls back to the plain
/// overdue-task reminder (`COND_TASK_OVERDUE`, preserving pre-existing baseline behavior).
pub fn detect_task_conditions(
    overdue_tasks: &[&Task],
    upcoming_events: &[CalendarEvent],
) -> Vec<NudgeCandidate> {
    overdue_tasks
        .iter()
        .map(|t| {
            let due_date = t.due_at.as_deref().and_then(|d| d.get(..10)).unwrap_or("?");
            let linked_event = t
                .calendar_event_id
                .as_deref()
                .and_then(|id| upcoming_events.iter().find(|e| e.id == id));
            match linked_event {
                Some(e) => {
                    let start = e.start_at.get(11..16).unwrap_or(&e.start_at);
                    NudgeCandidate {
                        condition: COND_OVERDUE_BLOCKS_MEETING,
                        entity_id: t.id.clone(),
                        title: "⚠️ Task quá hạn ảnh hưởng lịch hẹn".to_string(),
                        body: format!(
                            "[{}] {} quá hạn — liên quan '{}' lúc {start}",
                            t.priority, t.title, e.title
                        ),
                        urgent: true,
                    }
                }
                None => NudgeCandidate {
                    condition: COND_TASK_OVERDUE,
                    entity_id: t.id.clone(),
                    title: "⚠️ Task quá hạn".to_string(),
                    body: format!("[{}] {} — hạn {due_date}", t.priority, t.title),
                    urgent: matches!(t.priority.as_str(), "urgent" | "high"),
                },
            }
        })
        .collect()
}
