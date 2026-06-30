CREATE TABLE IF NOT EXISTS reminders (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    fire_at     TEXT NOT NULL,
    recurrence  TEXT,
    fired_at    TEXT,
    outcome     TEXT,
    outcome_at  TEXT,
    session_id  TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    deleted_at  TEXT
);
-- Scheduler query: WHERE fire_at <= now AND fired_at IS NULL AND deleted_at IS NULL
CREATE INDEX IF NOT EXISTS idx_reminders_pending ON reminders(fire_at)
    WHERE fired_at IS NULL AND deleted_at IS NULL;
