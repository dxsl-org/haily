CREATE TABLE IF NOT EXISTS tasks (
    id                TEXT PRIMARY KEY,
    title             TEXT NOT NULL,
    description       TEXT,
    priority          TEXT NOT NULL DEFAULT 'medium',
    status            TEXT NOT NULL DEFAULT 'todo',
    due_at            TEXT,
    completed_at      TEXT,
    calendar_event_id TEXT,
    domain_id         TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    deleted_at        TEXT
);
CREATE INDEX IF NOT EXISTS idx_tasks_active ON tasks(status, priority, due_at)
    WHERE status NOT IN ('done', 'cancelled') AND deleted_at IS NULL;

CREATE VIRTUAL TABLE IF NOT EXISTS tasks_fts USING fts5(
    title, description,
    content='tasks', content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 0'
);

CREATE TRIGGER IF NOT EXISTS tasks_ai AFTER INSERT ON tasks BEGIN
    INSERT INTO tasks_fts(rowid, title, description)
    VALUES (new.rowid, new.title, new.description);
END;

CREATE TRIGGER IF NOT EXISTS tasks_ad AFTER DELETE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, title, description)
    VALUES ('delete', old.rowid, old.title, old.description);
END;

CREATE TRIGGER IF NOT EXISTS tasks_au AFTER UPDATE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, title, description)
    VALUES ('delete', old.rowid, old.title, old.description);
    INSERT INTO tasks_fts(rowid, title, description)
    SELECT new.rowid, new.title, new.description
    WHERE new.deleted_at IS NULL AND new.status NOT IN ('done', 'cancelled');
END;
