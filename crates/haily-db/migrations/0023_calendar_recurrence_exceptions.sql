-- Phase 13b (assistant-depth): the exception model for occurrence-vs-series calendar
-- undo. A row here means "this ONE occurrence of this event is deleted" — the series
-- row itself (calendar_events) stays intact. 13a's `upcoming()` already subtracts
-- against this table (previously an always-empty in-memory set); this migration is
-- what makes that subtraction real.
--
-- UNIQUE(event_id, occurrence_start): an occurrence-delete is idempotent — deleting the
-- same occurrence twice must not create two rows (the forward write uses
-- `ON CONFLICT ... DO NOTHING`, detected as a no-op via rows_affected()==0).
CREATE TABLE IF NOT EXISTS calendar_exceptions (
    id                TEXT PRIMARY KEY,
    event_id          TEXT NOT NULL REFERENCES calendar_events(id),
    occurrence_start  TEXT NOT NULL,
    created_at        TEXT NOT NULL,
    UNIQUE(event_id, occurrence_start)
);
CREATE INDEX IF NOT EXISTS idx_calendar_exceptions_event ON calendar_exceptions(event_id);
