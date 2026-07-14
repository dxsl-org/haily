-- Additive nullable run_id on coding_workspaces (Pipeline Activation & Wiring, phase 1). Links
-- an ephemeral coding workspace to the pipeline run that drove it, so the P6 worktree reaper can
-- join a workspace row back to its `pipeline_runs` row. NULL until the launcher stamps it AFTER
-- the run reaches a terminal/paused state (the run mints its own id mid-flight, so it cannot be
-- known at workspace-open time) and for any workspace never driven by a pipeline run at all.
--
-- Nullable, no default, no backfill — every existing row and every existing sqlx query is
-- unaffected (same additive idiom as 0025_journal_workspace_id / 0026_pipeline_runs.run_id).
ALTER TABLE coding_workspaces ADD COLUMN run_id TEXT;

CREATE INDEX idx_coding_workspaces_run_id ON coding_workspaces(run_id);
