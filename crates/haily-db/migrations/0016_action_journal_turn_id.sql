-- Correlation column so every journal row from ONE agent turn can be undone as a group
-- via the already-shipped `batch_undo` (Safe Operator Harness — Harness Completion
-- phase 2, re-tier + turn undo). Nullable + additive: rows written before this migration
-- (and any local row whose caller predates turn-threading) simply have `turn_id = NULL`
-- and are excluded from any `list_by_turn` query, never mis-grouped.
--
-- Deliberately OUTSIDE the migration-0012 append-only trigger's explicit column list
-- (request_params, pre_state, pre_state_version, created_at, idempotency_key) — that
-- trigger enumerates columns by name, so adding a new column here does not implicitly
-- extend it. `turn_id` is still write-once in PRACTICE (set once at insert by
-- `NewAction`, never updated afterward by any query in this crate), it is just not
-- SQL-trigger-enforced the way the original evidentiary set is: unlike those columns
-- (raw secrets/PII whose immutability is a security property), a wrong/missing turn_id
-- has no tamper-evidence requirement — worst case is a row falls out of its turn's undo
-- group, which is the same no-op as if it were never journaled.
ALTER TABLE action_journal ADD COLUMN turn_id TEXT;

CREATE INDEX IF NOT EXISTS idx_action_journal_turn
    ON action_journal(turn_id, session_id);
