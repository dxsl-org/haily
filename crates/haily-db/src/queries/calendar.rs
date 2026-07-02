use crate::DbHandle;
use anyhow::Result;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct CalendarEvent {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_at: String,
    pub end_at: String,
    pub all_day: i64,
    pub recurrence: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

/// Fields for creating a calendar event. Grouped into a struct to keep
/// `insert` within a sane arity and to make call sites self-documenting.
pub struct NewCalendarEvent<'a> {
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub location: Option<&'a str>,
    pub start_at: &'a str,
    pub end_at: &'a str,
    pub all_day: bool,
    pub recurrence: Option<&'a str>,
}

pub async fn insert(db: &DbHandle, event: NewCalendarEvent<'_>) -> Result<CalendarEvent> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, CalendarEvent>(
        "INSERT INTO calendar_events
             (id, title, description, location, start_at, end_at, all_day, recurrence,
              created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(event.title)
    .bind(event.description)
    .bind(event.location)
    .bind(event.start_at)
    .bind(event.end_at)
    .bind(event.all_day as i64)
    .bind(event.recurrence)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}

/// Events starting between `from` and `to` (RFC3339 strings).
pub async fn upcoming(db: &DbHandle, from: &str, to: &str) -> Result<Vec<CalendarEvent>> {
    Ok(sqlx::query_as::<_, CalendarEvent>(
        "SELECT * FROM calendar_events
         WHERE start_at >= ? AND start_at <= ? AND deleted_at IS NULL
         ORDER BY start_at ASC",
    )
    .bind(from)
    .bind(to)
    .fetch_all(db.pool())
    .await?)
}

pub async fn soft_delete(db: &DbHandle, id: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE calendar_events SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL"
    )
    .bind(&now).bind(&now).bind(id)
    .execute(db.pool()).await?.rows_affected();
    Ok(rows > 0)
}

pub async fn get(db: &DbHandle, id: &str) -> Result<Option<CalendarEvent>> {
    Ok(sqlx::query_as::<_, CalendarEvent>(
        "SELECT * FROM calendar_events WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await?)
}
