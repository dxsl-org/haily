//! Pure detector-function tests — no DB, no event loop, no adapter.
use super::super::detectors::{
    detect_event_no_prep, detect_meeting_conditions, detect_task_conditions,
    COND_EVENT_NO_PREP, COND_MEETING_IMMINENT, COND_MEETING_PREP_INCOMPLETE,
    COND_OVERDUE_BLOCKS_MEETING, COND_TASK_OVERDUE,
};
use haily_db::queries::{calendar::CalendarEvent, tasks::Task};

fn event_fixture(id: &str, start_at: &str) -> CalendarEvent {
    CalendarEvent {
        id: id.into(),
        title: format!("event-{id}"),
        description: None,
        location: None,
        start_at: start_at.into(),
        end_at: start_at.into(),
        all_day: 0,
        recurrence: None,
        created_at: "2026-07-01T00:00:00Z".into(),
        updated_at: "2026-07-01T00:00:00Z".into(),
        deleted_at: None,
    }
}

fn task_fixture(id: &str, calendar_event_id: Option<&str>) -> Task {
    Task {
        id: id.into(),
        title: format!("task-{id}"),
        description: None,
        priority: "medium".into(),
        status: "todo".into(),
        due_at: None,
        completed_at: None,
        calendar_event_id: calendar_event_id.map(String::from),
        domain_id: None,
        created_at: "2026-07-01T00:00:00Z".into(),
        updated_at: "2026-07-01T00:00:00Z".into(),
        deleted_at: None,
    }
}

#[test]
fn meeting_detector_falls_back_to_generic_reminder_with_no_linked_task() {
    let event = event_fixture("e1", "2026-07-07T09:00:00Z");
    let candidates = detect_meeting_conditions(std::slice::from_ref(&event), &[]);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].condition, COND_MEETING_IMMINENT);
}

#[test]
fn meeting_detector_flags_an_incomplete_linked_prep_task() {
    let event = event_fixture("e5", "2026-07-07T09:00:00Z");
    let prep_task = task_fixture("t5", Some("e5"));
    let candidates = detect_meeting_conditions(std::slice::from_ref(&event), &[prep_task]);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].condition, COND_MEETING_PREP_INCOMPLETE);
}

#[test]
fn task_detector_distinguishes_plain_overdue_from_overdue_blocking_a_future_event() {
    let future_event = event_fixture("e2", "2026-07-10T09:00:00Z");
    let plain = task_fixture("t1", None);
    let blocking = task_fixture("t2", Some("e2"));
    let overdue: Vec<&Task> = vec![&plain, &blocking];

    let candidates = detect_task_conditions(&overdue, std::slice::from_ref(&future_event));
    assert_eq!(candidates.len(), 2);
    assert!(candidates.iter().any(|c| c.condition == COND_TASK_OVERDUE && c.entity_id == "t1"));
    assert!(
        candidates
            .iter()
            .any(|c| c.condition == COND_OVERDUE_BLOCKS_MEETING && c.entity_id == "t2")
    );
}

#[test]
fn event_no_prep_detector_ignores_events_with_any_linked_task_regardless_of_status() {
    let event = event_fixture("e3", "2026-07-07T09:00:00Z");
    let done_prep = task_fixture("t3", Some("e3"));
    assert!(detect_event_no_prep(std::slice::from_ref(&event), &[done_prep]).is_empty());

    let candidates = detect_event_no_prep(std::slice::from_ref(&event), &[]);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].condition, COND_EVENT_NO_PREP);
}
