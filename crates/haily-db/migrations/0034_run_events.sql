-- RunEvent persistence (Unified Chat UI phase 5, D2): the ordered observability stream a
-- pipeline run emits (haily_types::RunEvent) survives a restart so a Runs-history screen
-- can rehydrate a finished/interrupted run's timeline. `id` is an AUTOINCREMENT insertion-order
-- key — deliberately NOT `created_at`: only StageOutput carries a `seq` field and it is the one
-- variant never row-persisted here (see below), so two other-variant rows can share a
-- `created_at` tick and would replay out of order under a timestamp sort (e.g. RunComplete
-- before its own GateResult).
--
-- StageOutput's raw `chunk` text is DELIBERATELY NOT STORED here: it is arbitrary tool/model
-- output that routinely contains file contents, `.env` reads, or connector responses, and the
-- backup worker's credential scrub is a keyed DELETE over specific preference rows, not a
-- content scan — a raw-chunk column would leak past it into every GFS backup and
-- `export_database`. `run_stage_marker` below is the text-free substitute: one row per
-- (run, stage) with a running count + the last seen `seq`, enough for a UI preview count with
-- nothing to scrub.
--
-- Persistence is BEST-EFFORT (written by the per-run event bridge AFTER delivery to the live
-- adapter) — a crash between the two can leave the timeline missing its terminal RunComplete
-- row; the read path reconciles this against `pipeline_runs.status` (the authoritative source)
-- rather than trusting this table's completeness on its own.
CREATE TABLE IF NOT EXISTS run_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id     TEXT NOT NULL,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_run_events_run ON run_events(run_id, id);

-- One row per (run_id, stage): a text-free preview of a stage's StageOutput volume. Upserted by
-- the same bridge on every StageOutput chunk (count += 1, last_seq = the chunk's seq) — no
-- chunk text ever reaches this table either.
CREATE TABLE IF NOT EXISTS run_stage_marker (
    run_id     TEXT NOT NULL,
    stage      TEXT NOT NULL,
    count      INTEGER NOT NULL DEFAULT 0,
    last_seq   INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (run_id, stage)
);
