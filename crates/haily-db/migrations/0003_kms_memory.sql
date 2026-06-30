CREATE TABLE IF NOT EXISTS kms_episodic (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL,
    summary     TEXT NOT NULL,
    key_topics  TEXT,
    embedding   BLOB,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    deleted_at  TEXT
);
CREATE INDEX IF NOT EXISTS idx_episodic_session ON kms_episodic(session_id, created_at);

CREATE TABLE IF NOT EXISTS kms_skills (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL,
    pattern      TEXT NOT NULL,
    steps        TEXT NOT NULL,
    confidence   REAL NOT NULL DEFAULT 1.0,
    use_count    INTEGER NOT NULL DEFAULT 0,
    last_used_at TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    deleted_at   TEXT,
    archived_at  TEXT
);

CREATE TABLE IF NOT EXISTS kms_task_traces (
    id               TEXT PRIMARY KEY,
    session_id       TEXT NOT NULL,
    task_description TEXT NOT NULL,
    tool_calls       TEXT NOT NULL,
    outcome          TEXT NOT NULL,
    duration_ms      INTEGER,
    created_at       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_traces_session ON kms_task_traces(session_id, created_at);
