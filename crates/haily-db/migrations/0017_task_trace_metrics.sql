-- Harness Completion phase 5: per-turn telemetry + label provenance columns on
-- `kms_task_traces` (0003). Additive-only (nullable, no backfill needed) — a row
-- written before this migration simply has every new column NULL, which every
-- reader here treats as "unknown/not measured," never a fabricated zero.
--
-- `label_source`/`label_confidence` are the anti-reinforcement seam (researcher-03
-- §2): `label_source IS NULL` means "no signal fired" and MUST never drive
-- `update_skill_confidence` (see `haily-kms::skills::derive_label` — `unknown` skips
-- the EMA call entirely rather than defaulting to a neutral reward).
ALTER TABLE kms_task_traces ADD COLUMN model_tier TEXT;
ALTER TABLE kms_task_traces ADD COLUMN prompt_tokens INTEGER;
ALTER TABLE kms_task_traces ADD COLUMN completion_tokens INTEGER;
ALTER TABLE kms_task_traces ADD COLUMN tool_call_count INTEGER;
ALTER TABLE kms_task_traces ADD COLUMN approval_requested INTEGER;
ALTER TABLE kms_task_traces ADD COLUMN approval_denied INTEGER;
ALTER TABLE kms_task_traces ADD COLUMN undo_within_5min INTEGER;
ALTER TABLE kms_task_traces ADD COLUMN label_source TEXT;
ALTER TABLE kms_task_traces ADD COLUMN label_confidence REAL;
ALTER TABLE kms_task_traces ADD COLUMN delegate_overhead_ms INTEGER;

-- Rollup/aggregation reads filter and group by outcome (see 0018's rollup job) —
-- an index here keeps that a cheap scan instead of a full table scan as raw rows
-- accumulate toward the 90-day retention cap.
CREATE INDEX IF NOT EXISTS idx_task_traces_outcome ON kms_task_traces(outcome);
