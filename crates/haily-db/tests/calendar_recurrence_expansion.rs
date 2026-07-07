//! Phase 13a: `calendar::upcoming` expands `recurrence` across the query window instead of
//! surfacing a recurring event only at its stored `start_at`. Reuses `RecurrenceRule` from
//! `haily_db::recurrence` (Phase 02) — no forked recurrence logic lives here.
use haily_db::{queries::calendar, DbHandle};

async fn setup() -> (DbHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = DbHandle::init(&db_path).await.unwrap();
    (db, dir)
}

#[tokio::test]
async fn weekly_recurring_event_surfaces_on_every_monday_in_window() {
    let (db, _dir) = setup().await;
    // Event started 3 weeks before the query window; weekly:mon recurrence.
    calendar::insert(
        &db,
        calendar::NewCalendarEvent {
            title: "standup",
            description: None,
            location: None,
            start_at: "2025-12-15T09:00:00+00:00", // a Monday
            end_at: "2025-12-15T09:30:00+00:00",
            all_day: false,
            recurrence: Some("weekly:mon"),
        },
    )
    .await
    .unwrap();

    // 4-week window starting the first Monday after the event's stored start.
    let events = calendar::upcoming(&db, "2026-01-05T00:00:00+00:00", "2026-01-27T23:59:59+00:00")
        .await
        .unwrap();

    let starts: Vec<&str> = events.iter().map(|e| e.start_at.as_str()).collect();
    assert_eq!(
        starts,
        vec![
            "2026-01-05T09:00:00+00:00",
            "2026-01-12T09:00:00+00:00",
            "2026-01-19T09:00:00+00:00",
            "2026-01-26T09:00:00+00:00",
        ]
    );
    // Every occurrence keeps the underlying event's id — the join key phase 13b scopes
    // occurrence-vs-series undo on.
    let ids: std::collections::HashSet<&str> = events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids.len(), 1);
    // end_at shifts by the same duration as the original occurrence (30 minutes).
    assert_eq!(events[0].end_at, "2026-01-05T09:30:00+00:00");
}

#[tokio::test]
async fn non_recurring_event_surfaces_exactly_once() {
    let (db, _dir) = setup().await;
    calendar::insert(
        &db,
        calendar::NewCalendarEvent {
            title: "one-off meeting",
            description: None,
            location: None,
            start_at: "2026-02-10T14:00:00+00:00",
            end_at: "2026-02-10T15:00:00+00:00",
            all_day: false,
            recurrence: None,
        },
    )
    .await
    .unwrap();

    let events = calendar::upcoming(&db, "2026-02-01T00:00:00+00:00", "2026-02-28T23:59:59+00:00")
        .await
        .unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].start_at, "2026-02-10T14:00:00+00:00");
}

#[tokio::test]
async fn recurring_event_outside_window_does_not_surface() {
    let (db, _dir) = setup().await;
    calendar::insert(
        &db,
        calendar::NewCalendarEvent {
            title: "future series",
            description: None,
            location: None,
            start_at: "2026-06-01T09:00:00+00:00",
            end_at: "2026-06-01T09:30:00+00:00",
            all_day: false,
            recurrence: Some("daily"),
        },
    )
    .await
    .unwrap();

    let events = calendar::upcoming(&db, "2026-01-01T00:00:00+00:00", "2026-01-31T23:59:59+00:00")
        .await
        .unwrap();

    assert!(events.is_empty(), "series starting after the window must not appear");
}

#[tokio::test]
async fn recurring_event_expansion_is_bounded_by_the_per_event_cap() {
    let (db, _dir) = setup().await;
    // Daily event over a multi-year window — expansion must not grow unboundedly (DoS guard).
    calendar::insert(
        &db,
        calendar::NewCalendarEvent {
            title: "daily habit",
            description: None,
            location: None,
            start_at: "2024-01-01T07:00:00+00:00",
            end_at: "2024-01-01T07:15:00+00:00",
            all_day: false,
            recurrence: Some("daily"),
        },
    )
    .await
    .unwrap();

    let events = calendar::upcoming(&db, "2024-01-01T00:00:00+00:00", "2026-12-31T23:59:59+00:00")
        .await
        .unwrap();

    // MAX_OCCURRENCES_PER_EVENT (366) — see haily_db::recurrence.
    assert_eq!(events.len(), 366);
}

#[tokio::test]
async fn mixed_recurring_and_non_recurring_events_are_sorted_together() {
    let (db, _dir) = setup().await;
    calendar::insert(
        &db,
        calendar::NewCalendarEvent {
            title: "weekly review",
            description: None,
            location: None,
            start_at: "2026-03-02T10:00:00+00:00", // a Monday
            end_at: "2026-03-02T11:00:00+00:00",
            all_day: false,
            recurrence: Some("weekly:mon"),
        },
    )
    .await
    .unwrap();
    calendar::insert(
        &db,
        calendar::NewCalendarEvent {
            title: "dentist",
            description: None,
            location: None,
            start_at: "2026-03-04T15:00:00+00:00",
            end_at: "2026-03-04T15:30:00+00:00",
            all_day: false,
            recurrence: None,
        },
    )
    .await
    .unwrap();

    let events = calendar::upcoming(&db, "2026-03-01T00:00:00+00:00", "2026-03-16T23:59:59+00:00")
        .await
        .unwrap();

    let starts: Vec<&str> = events.iter().map(|e| e.start_at.as_str()).collect();
    assert_eq!(
        starts,
        vec![
            "2026-03-02T10:00:00+00:00",
            "2026-03-04T15:00:00+00:00",
            "2026-03-09T10:00:00+00:00",
            "2026-03-16T10:00:00+00:00",
        ]
    );
}
