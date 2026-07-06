-- Phase 8 (Activate & Measure): approval-prompt outcomes aggregated into the daily
-- rollup. Per-turn `approval_requested`/`approval_denied` are already recorded on
-- `kms_task_traces` (migration 0017, populated by `record_outcome_and_update_skill`'s
-- `approval_stats` call) — this migration only adds the SUMMED daily counters so a
-- day's approval activity is readable without scanning raw traces (which are pruned
-- past `RAW_RETENTION_DAYS`, at which point the rollup is the only surviving record).
-- Additive + backfill-safe: `DEFAULT 0` means a rollup row written before this
-- migration reads back as a real zero, not NULL — correct here (unlike the token/
-- outcome columns) because "no approval was requested that day" and "we never
-- measured approvals that day" happen to coincide for every pre-existing row (the
-- per-turn source columns this SUMs were always non-NULL booleans once migration
-- 0017 landed, so re-running `compute_daily_rollup` for old dates yields the true sum).
ALTER TABLE kms_daily_rollup ADD COLUMN approval_requested_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE kms_daily_rollup ADD COLUMN approval_denied_count INTEGER NOT NULL DEFAULT 0;
