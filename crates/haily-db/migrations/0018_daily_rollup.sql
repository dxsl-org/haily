-- Harness Completion phase 5: daily aggregation of `kms_task_traces`, collapsing raw
-- per-turn rows older than the retention window into one row per (date, model_tier).
-- `model_tier` is NOT NULL with a `''` sentinel for "no tier recorded" (query layer
-- normalizes a NULL trace value to `''` before grouping) — SQLite's UNIQUE index
-- treats NULL as distinct per row (never deduped), which would silently multiply
-- "unknown tier" rollup rows on every rerun; a NOT NULL sentinel keeps the upsert's
-- ON CONFLICT target meaningful.
CREATE TABLE IF NOT EXISTS kms_daily_rollup (
    id                TEXT PRIMARY KEY,
    date              TEXT NOT NULL,
    model_tier        TEXT NOT NULL DEFAULT '',
    count             INTEGER NOT NULL,
    success_count     INTEGER NOT NULL,
    partial_count     INTEGER NOT NULL,
    failure_count     INTEGER NOT NULL,
    unknown_count     INTEGER NOT NULL,
    avg_duration_ms   REAL,
    avg_prompt_tokens REAL,
    avg_completion_tokens REAL,
    undo_count        INTEGER NOT NULL,
    created_at        TEXT NOT NULL
);

-- One row per (date, model_tier) — the rollup job upserts on this pair so a rerun
-- (e.g. a worker restart mid-cycle) accumulates correctly instead of duplicating.
CREATE UNIQUE INDEX IF NOT EXISTS idx_daily_rollup_date_tier
    ON kms_daily_rollup(date, model_tier);
