-- Sub-Agent + Skill Architecture phase 1: tag every coding-tool journal row with the
-- CodingWorkspace it wrote inside, so the JournalBrowser can group a pipeline run's file
-- changes and a future GC can attribute audit rows to a workspace. `NULL` for every
-- non-coding row (local personal tools, connector writes) and any row written before this
-- migration — those paths never set it.
--
-- This is EVIDENTIARY, like manifest_hash (migration 0019): the workspace a change actually
-- executed in must never be rewritten after the fact, so it joins the migration-0012
-- append-only trigger's protected column list. SQLite triggers cannot be ALTERed, so the
-- existing trigger is dropped and recreated with the extended list — behaviorally additive;
-- the previously-guarded columns keep their exact prior semantics. This does NOT touch the
-- turn_id / session_id scoping that `undo_turn` / `batch_undo` rely on.
ALTER TABLE action_journal ADD COLUMN workspace_id TEXT;

DROP TRIGGER IF EXISTS action_journal_no_update;

CREATE TRIGGER action_journal_no_update
BEFORE UPDATE OF
    request_params, pre_state, pre_state_version, created_at, idempotency_key,
    manifest_hash, workspace_id
ON action_journal
FOR EACH ROW
WHEN
    OLD.request_params    IS NOT NEW.request_params
    OR OLD.pre_state      IS NOT NEW.pre_state
    OR OLD.pre_state_version IS NOT NEW.pre_state_version
    OR OLD.created_at     IS NOT NEW.created_at
    OR OLD.idempotency_key IS NOT NEW.idempotency_key
    OR OLD.manifest_hash  IS NOT NEW.manifest_hash
    OR OLD.workspace_id   IS NOT NEW.workspace_id
BEGIN
    SELECT RAISE(ABORT, 'action_journal: evidentiary columns are append-only');
END;
