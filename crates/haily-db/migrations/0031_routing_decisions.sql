-- Routing decision log (Auto Model Routing R1, phase 2). IS the R2 training set: judge-graft
-- ordering puts the log ahead of any routing *behavior* change (Phases 4/6 write the rows this
-- schema receives; this migration only creates the table + join column).
--
-- Write contract (red-team, phase-02 requirement): one row per unit of work (a chat turn or a
-- pipeline stage), written best-effort at unit END, never on the pre-first-token hot path, and
-- never with `?` — a telemetry write must not abort a chat turn. A crashed unit writes nothing;
-- it produced no outcome to train on.
--
-- Only derived features are stored (word counts, booleans, enum labels) — never raw message
-- text, both for privacy and to keep model-authored text out of a table that other model calls
-- read back (prompt-injection surface).
CREATE TABLE routing_decisions (
    id                          TEXT PRIMARY KEY,
    -- Join key back to kms_task_traces / action_journal for the same unit of work.
    turn_id                     TEXT NOT NULL,
    -- Pipeline run id (P4/P6 stages only); NULL for a plain chat turn.
    run_id                      TEXT,
    -- 'chat' | 'pipeline_stage'.
    context_kind                TEXT NOT NULL,
    -- Pipeline stage name (e.g. 'build', 'review'); NULL for chat.
    stage_kind                  TEXT,
    -- 'fast' | 'medium' | 'thinking' | 'ultra' | NULL (session default was used).
    chosen_tier                 TEXT,
    -- Tier the unit escalated to mid-flight; NULL means no escalation happened.
    escalated_to                TEXT,
    -- 'default' | 'heuristic' | 'explicit_phrase' | 'depth'.
    decision_source             TEXT NOT NULL,
    cost_quality                INTEGER NOT NULL,
    feature_msg_words           INTEGER NOT NULL,
    -- Boolean (0/1): did the triggering message contain a code block/fence.
    feature_has_code            INTEGER NOT NULL,
    -- Count of PRIOR USER messages in context — a trusted-origin signal. Tool output and
    -- assistant text must never feed routing decisions (prompt-injection surface), so this
    -- column is deliberately scoped to user-authored turns only.
    feature_history_user_msgs   INTEGER NOT NULL,
    -- DepthMode label ('quick' | 'normal' | 'deep').
    feature_depth               TEXT NOT NULL,
    -- NULL | 'stream_init_error' | 'gate_failure' — what forced a mid-unit escalation.
    escalation_trigger          TEXT,
    prior_failures              INTEGER NOT NULL DEFAULT 0,
    created_at                  TEXT NOT NULL
);

CREATE INDEX idx_routing_decisions_turn ON routing_decisions(turn_id);

-- R2 join key (researcher-03): turn_id is minted in haily-core::agent::turn but never reached
-- kms_task_traces before this migration. Additive/nullable so existing rows and existing sqlx
-- queries are unaffected (same idiom as 0025_journal_workspace_id, 0026_pipeline_runs.run_id).
ALTER TABLE kms_task_traces ADD COLUMN turn_id TEXT;
