-- Append-only journal of every connector write, recorded BEFORE the external call
-- (outbox pattern) so a crash mid-write still leaves the compensation_plan + pre_state
-- on disk for reconciliation. This is the ENGINE behind the undo/kill-switch/reconcile
-- state machine (Safe Operator Harness phase 3).
--
-- tool_name is a DENORMALIZED string, NOT an FK to connector_manifests: 0012 precedes
-- 0013 (manifests), and an FK would force 0013-before-0012 and fight the append-only
-- trigger below. A journal row must survive even if its manifest is later removed.
CREATE TABLE IF NOT EXISTS action_journal (
    id                   TEXT PRIMARY KEY,
    session_id           TEXT NOT NULL,
    -- Denormalized connector tool name (see table note). Not an FK.
    tool_name            TEXT NOT NULL,
    -- RiskTier string at the time of the write ('IrreversibleWrite' etc.).
    tool_tier            TEXT NOT NULL,
    -- read | reversible | compensatable | final — drives undo refusal on 'final'.
    compensability       TEXT NOT NULL,
    -- Minted once per logical op when the LLM issues it, reused only on retry of THAT
    -- op. UNIQUE so a duplicate submit of the same op is a conflict, not a second write.
    idempotency_key      TEXT NOT NULL UNIQUE,
    -- Client-generated ref written into create payloads so a lost response (C7) can be
    -- reconciled by search_read on this ref rather than blind-retrying a create.
    correlation_ref      TEXT NOT NULL,
    -- REDACTED (C4): the Odoo key positional + Authorization/Cookie headers are stripped
    -- before insert; a credential *reference* (preference key name) is stored instead.
    request_params       TEXT NOT NULL,
    -- Third-party record content, tag-stripped (C5) before insert.
    pre_state            TEXT,
    -- Opaque version token (Odoo write_date — C10). Re-read before compensating an
    -- update; undo refuses on change (concurrency guard, distinct from shape read-back).
    pre_state_version    TEXT,
    post_state           TEXT,
    -- pending | match | mismatch | skipped | unknown | unverified
    readback_status      TEXT NOT NULL DEFAULT 'pending',
    -- JSON compensation plan, written BEFORE the external call (outbox).
    compensation_plan    TEXT,
    -- State machine: not_requested | undo_requested | refused | compensating |
    -- undone | compensation_failed | stuck
    undo_status          TEXT NOT NULL DEFAULT 'not_requested',
    undo_attempts        INTEGER NOT NULL DEFAULT 0,
    created_at           TEXT NOT NULL,
    undone_at            TEXT,
    -- PII bound: request_params/pre_state/post_state are purged past this timestamp by
    -- the WIRED purge_expired job (bootstrap interval task).
    retention_expires_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_action_journal_session
    ON action_journal(session_id, created_at);

CREATE INDEX IF NOT EXISTS idx_action_journal_retention
    ON action_journal(retention_expires_at);

-- Append-only guard: EVIDENTIARY columns can never be rewritten by the app's own SQL
-- layer (sufficient for the single-user local threat model; no adversarial-tamper model,
-- hash-chaining is YAGNI). Processing columns (undo_status/readback_status/undo_attempts/
-- undone_at/post_state) stay mutable so the state machine + read-back can advance.
--
-- NO blanket DELETE trigger: purge_expired + sqlx migrations must be able to remove rows.
-- If tamper-evidence is later wanted, exempt the purge path explicitly rather than
-- reintroducing a blanket block.
CREATE TRIGGER IF NOT EXISTS action_journal_no_update
BEFORE UPDATE OF
    request_params, pre_state, pre_state_version, created_at, idempotency_key
ON action_journal
FOR EACH ROW
WHEN
    OLD.request_params    IS NOT NEW.request_params
    OR OLD.pre_state      IS NOT NEW.pre_state
    OR OLD.pre_state_version IS NOT NEW.pre_state_version
    OR OLD.created_at     IS NOT NEW.created_at
    OR OLD.idempotency_key IS NOT NEW.idempotency_key
BEGIN
    SELECT RAISE(ABORT, 'action_journal: evidentiary columns are append-only');
END;
