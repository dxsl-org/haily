-- Pipeline runs (Sub-Agent + Skill Architecture phase 4). One row per pipeline RUN: the
-- persistent, resumable, cancellable record of a Rust-native stage machine executing an
-- ordered set of `Stage`s inside a CodingWorkspace. The runner (P4b) writes a stage
-- transition here in the SAME DB transaction as the matching action_journal row so a crash
-- can never leave the journal↔run pair inconsistent (red-team FMA-C2).
--
-- The authoritative pipeline-global liveness bound is `attempts_remaining` (red-team
-- FMA-C1): it is decremented across attempts and SURVIVES restart, unlike the per-turn
-- LoopGuard (resets on each stage's fresh turn_id) or a wall-clock (re-arms on every
-- approval pulse). A run is never auto-resumed after `interrupted` — resume of ANY stage
-- requires explicit user action (FMA-m4).
--
-- soft-delete via `deleted_at` mirrors work_items / coding_workspaces (migration 0024): a
-- discarded run stays as an evidentiary row rather than being hard-removed.
--
-- Forward columns (`findings`, `per_attempt_tokens`) are declared nullable NOW so P6
-- (findings) and P8 (per-attempt token accounting) need no further migration on this table
-- (DEP-minor). They are unused until those phases wire writers.
CREATE TABLE IF NOT EXISTS pipeline_runs (
    id                 TEXT PRIMARY KEY,
    -- Optional owning work_item (a run surfaces as a long-running work_item); NULL for an
    -- ad-hoc run not tied to a tracked item.
    work_item_id       TEXT REFERENCES work_items(id),
    session_id         TEXT NOT NULL REFERENCES sessions(id),
    -- 0-based index of the stage currently executing / last executed.
    stage_index        INTEGER NOT NULL DEFAULT 0,
    -- queued / running / paused / interrupted / done / failed (see pipeline::RunStatus).
    status             TEXT NOT NULL,
    -- Attempt counter for the current stage (0-based); advances on each verifier-grounded retry.
    attempt            INTEGER NOT NULL DEFAULT 0,
    -- Persistent pipeline-global liveness bound (FMA-C1); decremented across attempts.
    attempts_remaining INTEGER NOT NULL DEFAULT 0,
    -- Resolved tier / backend the last stage ran on (audit + escalation observability).
    tier_used          TEXT,
    backend_used       TEXT,
    -- Whether the last stage's model call left the machine (local / cloud) — egress accounting.
    egress             TEXT,
    -- Short hash of the last gate's decisive output, for flaky-gate detection (FMA-M5).
    gate_output_digest TEXT,
    -- P6 forward column (nullable): synthesized run findings. Unused until P6.
    findings           TEXT,
    -- P8 forward column (nullable JSON): per-attempt token accounting. Unused until P8.
    per_attempt_tokens TEXT,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    deleted_at         TEXT
);

CREATE INDEX IF NOT EXISTS idx_pipeline_runs_session
    ON pipeline_runs(session_id, created_at);

-- Active-run lookup for resume-on-boot / reconcile (list_active, list_interrupted).
CREATE INDEX IF NOT EXISTS idx_pipeline_runs_status
    ON pipeline_runs(status, deleted_at);

-- Correlation column so every journal row from ONE pipeline run can be undone as a group via
-- the already-shipped `batch_undo` (mirrors turn_id, migration 0016). Nullable + additive:
-- rows written before this migration, and every non-pipeline row, have `run_id = NULL` and
-- are excluded from any `list_by_run` query, never mis-grouped.
--
-- Deliberately OUTSIDE the migration-0012 append-only trigger (like turn_id, unlike
-- workspace_id/manifest_hash): a wrong/missing run_id has no tamper-evidence requirement —
-- worst case a row falls out of its run's undo group, the same no-op as never being
-- journaled. It is write-once in PRACTICE (set at insert, never updated).
ALTER TABLE action_journal ADD COLUMN run_id TEXT;

CREATE INDEX IF NOT EXISTS idx_action_journal_run
    ON action_journal(run_id, session_id);
