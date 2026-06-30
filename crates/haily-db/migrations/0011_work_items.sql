-- Work items track multi-step agent turns so the user can see what's in progress,
-- paused, or interrupted across sessions.
CREATE TABLE IF NOT EXISTS work_items (
    id           TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL,
    title        TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'queued',
    phase        TEXT,
    progress     INTEGER NOT NULL DEFAULT 0,
    checkpoint   TEXT,
    started_at   TEXT,
    completed_at TEXT,
    error        TEXT,
    created_at   TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at   TEXT NOT NULL DEFAULT (datetime('now')),
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);

CREATE INDEX IF NOT EXISTS idx_work_items_active
    ON work_items(status, started_at)
    WHERE status IN ('running', 'paused', 'queued', 'interrupted');
