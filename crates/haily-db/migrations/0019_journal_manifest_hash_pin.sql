-- Activate-and-Measure phase 4b (M2): pin the connector manifest's content hash into the
-- journal row AT OUTBOX-INSERT TIME so undo/reconcile can detect a manifest that moved its
-- base_url (or was re-approved with different ops/auth) BETWEEN the forward write and the
-- compensation. Without this, a compensation write — and the credential it carries — could
-- be sent to a base_url/schema the original forward write never touched. `NULL` for a local
-- row (no connector manifest at all) and for any row written before this migration; both
-- cases skip the hash comparison entirely (see `journal_undo::ConnectorResolver::hash_matches`).
--
-- Evidentiary, like request_params/pre_state: the hash the write ACTUALLY executed against
-- must never be rewritten after the fact, so it joins the migration-0012 append-only
-- trigger's protected column list. SQLite triggers cannot be ALTERed, so the existing
-- trigger is dropped and recreated with the extended list — behaviorally additive; the four
-- originally-guarded columns keep their exact prior semantics.
ALTER TABLE action_journal ADD COLUMN manifest_hash TEXT;

DROP TRIGGER IF EXISTS action_journal_no_update;

CREATE TRIGGER action_journal_no_update
BEFORE UPDATE OF
    request_params, pre_state, pre_state_version, created_at, idempotency_key, manifest_hash
ON action_journal
FOR EACH ROW
WHEN
    OLD.request_params    IS NOT NEW.request_params
    OR OLD.pre_state      IS NOT NEW.pre_state
    OR OLD.pre_state_version IS NOT NEW.pre_state_version
    OR OLD.created_at     IS NOT NEW.created_at
    OR OLD.idempotency_key IS NOT NEW.idempotency_key
    OR OLD.manifest_hash  IS NOT NEW.manifest_hash
BEGIN
    SELECT RAISE(ABORT, 'action_journal: evidentiary columns are append-only');
END;
