-- Phase 11 (assistant-depth): work_items closes the harness gap it previously had
-- (no deleted_at column, hence no journal/undo coverage for a destructive mutation).
-- Purely additive: existing rows get deleted_at = NULL (untouched/active).
ALTER TABLE work_items ADD COLUMN deleted_at TEXT;

-- Recreate the active-item index so a soft-deleted row never surfaces in the
-- watcher/list queries it backs (mirrors tasks/notes/reminders' own active index).
DROP INDEX IF EXISTS idx_work_items_active;
CREATE INDEX IF NOT EXISTS idx_work_items_active
    ON work_items(status, started_at)
    WHERE status IN ('running', 'paused', 'queued', 'interrupted') AND deleted_at IS NULL;
