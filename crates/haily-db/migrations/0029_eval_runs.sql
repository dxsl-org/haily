-- Eval runs (Sub-Agent + Skill Architecture phase 9, Golden Coding Eval). One row per scored
-- eval task run: the measurement schema the long-gated Router A/B experiment needs and the
-- escalation-default (P3) decision reads its data from.
--
-- MIGRATION NUMBER = 0029 (next sequential after 0028_review_findings), NOT the 0028 the P9
-- plan text pre-allocated: migrations are applied in strict version order by `sqlx::migrate!`
-- and P8 (review_findings) landed FIRST, so it correctly took 0028 (its own deviation-log
-- entry records this). P9's eval_runs is therefore 0029. (P9 deviation-log entry.)
--
-- `task_kind` ('coding' | 'automation') is declared NOW so P14's AutomationBench-modeled eval
-- reuses this exact table with NO new migration (DEP-minor) — same idiom as the pipeline_runs
-- forward columns.
--
-- Per-attempt egress (`egress`, red-team FMA-M2) is recorded so an escalation-`on` LOCAL
-- baseline that silently crossed to cloud is VISIBLE per attempt — rows stay tagged, the Router
-- A/B signal stays clean. `per_stage_tokens` and `gate_results` are JSON blobs the report
-- renderer reads back; they are opaque to SQL.
--
-- soft-delete via `deleted_at` mirrors work_items / pipeline_runs: a discarded eval stays as an
-- evidentiary row rather than being hard-removed.
CREATE TABLE IF NOT EXISTS eval_runs (
    id                TEXT PRIMARY KEY,
    -- The eval task this row scored (the fixture manifest `id`, e.g. 'rust-fix-compile').
    task_id           TEXT NOT NULL,
    -- 'coding' (P9) | 'automation' (P14). Lets both evals share this table.
    task_kind         TEXT NOT NULL DEFAULT 'coding',
    -- The model the baseline ran against (e.g. a local GGUF name or a cloud model id).
    model             TEXT NOT NULL,
    -- The tier configuration the run used (e.g. 'local' / 'local+escalate' / 'cloud').
    tier_config       TEXT NOT NULL,
    -- Judgment depth ('quick' | 'normal' | 'deep' — matches DepthMode::as_label).
    depth             TEXT NOT NULL,
    -- Per-stage token accounting (JSON array of {stage, attempt, tier, backend, prompt_tokens,
    -- completion_tokens}); mirrors pipeline_runs.per_attempt_tokens.
    per_stage_tokens  TEXT,
    -- Total retries/escalations across all stages of the run.
    escalation_count  INTEGER NOT NULL DEFAULT 0,
    -- Per-attempt egress tags (JSON array of {attempt, egress:'local'|'cloud'|'unknown'}) —
    -- FMA-M2: a local baseline arm that crossed to cloud is visible here, not hidden.
    egress            TEXT,
    -- Wall-clock of the whole run in milliseconds.
    wall_clock_ms     INTEGER NOT NULL DEFAULT 0,
    -- Deterministic gate verdict: 1 = every scoring gate passed, 0 = at least one failed.
    passed            INTEGER NOT NULL DEFAULT 0,
    -- The scored gate results (JSON array of {gate, pass, detail}) the report table renders.
    gate_results      TEXT,
    created_at        TEXT NOT NULL,
    deleted_at        TEXT
);

-- Router A/B reads by (model, tier_config, depth) across task rows; index the common cut.
CREATE INDEX IF NOT EXISTS idx_eval_runs_matrix
    ON eval_runs(task_kind, model, tier_config, depth);

-- Per-task history lookup (a fixture's pass-rate over time).
CREATE INDEX IF NOT EXISTS idx_eval_runs_task
    ON eval_runs(task_id, created_at);
