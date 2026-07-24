-- Run-control support (Unified Chat UI phase 6, D3): kill/pause/resume.
--
-- `task`/`run_kind`/`depth` let a paused/interrupted row's originating `CodingRunSpec` be
-- reconstructed for `resume_run`'s relaunch (any row within a multi-run launch — a phase
-- build, its review, a fix-loop retry, or the final ship — may end up the one that pauses or
-- is interrupted, not just the launch's first row). NULL for a row created before this
-- migration, or by a caller with no resume context (eval/test runs) — such a row is simply
-- never resumable, never an error.
ALTER TABLE pipeline_runs ADD COLUMN task TEXT;
ALTER TABLE pipeline_runs ADD COLUMN run_kind TEXT;
ALTER TABLE pipeline_runs ADD COLUMN depth TEXT;

-- Reason CLASS for a `paused` row — retries_exhausted | explicit_stop | awaiting_approval |
-- other — stamped by the runner at the moment `paused_reason` is set, never re-derived by
-- string-matching the free-text reason at resume time. `resume_run`'s guard reads only this
-- column (see `haily-app::run_control`).
ALTER TABLE pipeline_runs ADD COLUMN pause_reason_class TEXT;
