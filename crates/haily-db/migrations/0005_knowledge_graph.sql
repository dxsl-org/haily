CREATE TABLE IF NOT EXISTS entities (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    deleted_at  TEXT
);
CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(name) WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS entity_edges (
    id          TEXT PRIMARY KEY,
    from_id     TEXT NOT NULL,
    to_id       TEXT NOT NULL,
    predicate   TEXT NOT NULL,
    fact_id     TEXT,
    weight      REAL NOT NULL DEFAULT 1.0,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    deleted_at  TEXT,
    UNIQUE (from_id, to_id, predicate)
);
CREATE INDEX IF NOT EXISTS idx_edges_from ON entity_edges(from_id) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_edges_to   ON entity_edges(to_id)   WHERE deleted_at IS NULL;
