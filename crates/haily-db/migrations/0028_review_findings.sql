-- Review findings history (Sub-Agent + Skill Architecture phase 8, learning loop).
--
-- One row per persisted review Finding, so the pipeline's recurrence detector can spot a
-- RECURRING class of problem (≥2 same-class findings across runs) at Ship time and raise a
-- distillation PROPOSAL. This is distinct from `pipeline_runs.findings` (the per-run JSON blob
-- the Fix loop reads back within a single run): that column answers "what did THIS review
-- find"; this table answers "what keeps coming back across runs for this workspace".
--
-- Class key = (category, module): `category` is the finding severity today (start coarse, tune
-- with P9 data), `module` is the crate/module the finding's file lives in. Recurrence groups by
-- (session_id/workspace_id, category, module).
--
-- MIGRATION NUMBER = 0028 (next sequential after 0027), NOT the 0029 the plan pre-allocated:
-- migrations are built SEQUENTIALLY here and P8 precedes P9, so `sqlx::migrate!` (which applies
-- in version order and errors if an earlier-numbered file appears after a later one already ran)
-- requires review_findings be 0028. P9's eval_runs takes 0029 later. (P8 deviation-log entry.)
CREATE TABLE IF NOT EXISTS review_findings (
    id            TEXT PRIMARY KEY,
    -- The review run that produced this finding (audit link back to pipeline_runs).
    run_id        TEXT NOT NULL REFERENCES pipeline_runs(id),
    session_id    TEXT NOT NULL REFERENCES sessions(id),
    -- Owning coding workspace (nullable — a run may not be workspace-scoped). Recurrence is
    -- keyed by workspace when present, else by session.
    workspace_id  TEXT,
    -- Class key components. `category` = severity today; `module` = crate/module of `file`.
    category      TEXT NOT NULL,
    module        TEXT NOT NULL,
    -- The finding detail, for rendering the eventual distillation proposal (already
    -- tag-stripped upstream — never re-emitted as a live tool tag).
    severity      TEXT NOT NULL,
    file          TEXT NOT NULL DEFAULT '',
    summary       TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

-- Recurrence lookups group by (workspace/session, category, module).
CREATE INDEX IF NOT EXISTS idx_review_findings_class
    ON review_findings(session_id, category, module);
CREATE INDEX IF NOT EXISTS idx_review_findings_workspace_class
    ON review_findings(workspace_id, category, module);
