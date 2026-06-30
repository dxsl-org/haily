CREATE TABLE IF NOT EXISTS kms_feedback (
    id               TEXT PRIMARY KEY,
    session_id       TEXT NOT NULL,
    message_id       TEXT,
    reaction         TEXT NOT NULL,
    content          TEXT,
    affected_fact_id TEXT,
    created_at       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS kms_preferences (
    id          TEXT PRIMARY KEY,
    key         TEXT NOT NULL UNIQUE,
    value       TEXT NOT NULL,
    confidence  REAL NOT NULL DEFAULT 1.0,
    source      TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
