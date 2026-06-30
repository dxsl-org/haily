CREATE TABLE IF NOT EXISTS kms_domains (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    deleted_at  TEXT
);

CREATE TABLE IF NOT EXISTS kms_facts (
    id          TEXT PRIMARY KEY,
    domain_id   TEXT NOT NULL,
    subject     TEXT NOT NULL,
    predicate   TEXT NOT NULL,
    object      TEXT NOT NULL,
    confidence  REAL NOT NULL DEFAULT 1.0,
    source      TEXT NOT NULL,
    source_ref  TEXT,
    embedding   BLOB,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    deleted_at  TEXT,
    archived_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_facts_domain     ON kms_facts(domain_id, deleted_at, archived_at);
CREATE INDEX IF NOT EXISTS idx_facts_confidence ON kms_facts(confidence DESC) WHERE deleted_at IS NULL;

CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(
    subject, predicate, object,
    content='kms_facts', content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 0'
);

CREATE TRIGGER IF NOT EXISTS kms_facts_ai AFTER INSERT ON kms_facts BEGIN
    INSERT INTO facts_fts(rowid, subject, predicate, object)
    VALUES (new.rowid, new.subject, new.predicate, new.object);
END;

CREATE TRIGGER IF NOT EXISTS kms_facts_ad AFTER DELETE ON kms_facts BEGIN
    INSERT INTO facts_fts(facts_fts, rowid, subject, predicate, object)
    VALUES ('delete', old.rowid, old.subject, old.predicate, old.object);
END;

-- Keeps FTS in sync with soft-deletes and archiving
CREATE TRIGGER IF NOT EXISTS kms_facts_au AFTER UPDATE ON kms_facts BEGIN
    INSERT INTO facts_fts(facts_fts, rowid, subject, predicate, object)
    VALUES ('delete', old.rowid, old.subject, old.predicate, old.object);
    INSERT INTO facts_fts(rowid, subject, predicate, object)
    SELECT new.rowid, new.subject, new.predicate, new.object
    WHERE new.deleted_at IS NULL AND new.archived_at IS NULL;
END;
