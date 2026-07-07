-- Phase 4 (assistant-depth): persistent cooldown ledger for cross-domain nudges.
-- `cross_domain::alert_loop` previously de-duped fired nudges with an in-process
-- HashSet (reset on every daemon restart, re-spamming every still-open condition).
-- This table survives restarts: a nudge is claimed by (condition, entity_id, fired_on)
-- via an atomic INSERT OR IGNORE (see queries/nudge_ledger.rs::try_claim) and never
-- fires again for that day once claimed. `fired_on` is a local calendar date
-- (YYYY-MM-DD) rather than a timestamp — cooldown granularity is "once per day",
-- not a rolling window, so the natural key is the day, not an expiry instant.
CREATE TABLE IF NOT EXISTS nudge_cooldown_ledger (
    condition  TEXT NOT NULL,
    entity_id  TEXT NOT NULL,
    fired_on   TEXT NOT NULL,
    fired_at   TEXT NOT NULL,
    PRIMARY KEY (condition, entity_id, fired_on)
);
