-- Mobile thin-client paired-device registry (Mobile Thin-Client plan phase 2a).
-- One row per paired phone. `token_hash` is the SHA-256 hex of the device's bearer
-- token (`Authorization` header on the WS upgrade) — the plaintext token is returned
-- to the client exactly once, at pairing time (`PairResponse`), and never stored.
-- Soft-revoke via `revoked_at` mirrors the existing `deleted_at` convention
-- (CLAUDE.md: "soft-delete with deleted_at") rather than a hard DELETE, so a revoked
-- device's pairing history stays auditable.
--
-- `last_seen_at IS NULL` marks a device that completed `POST /pair` but never
-- actually connected over WS — `reap_unconfirmed` (queries/devices.rs) prunes these
-- after a grace window (red team m6) so an abandoned/failed pairing does not linger
-- forever as a phantom "paired" row.
CREATE TABLE IF NOT EXISTS devices (
    device_id     TEXT PRIMARY KEY,
    device_name   TEXT NOT NULL,
    token_hash    TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    last_seen_at  TEXT,
    revoked_at    TEXT
);

-- The WS upgrade auth path looks up a device by token hash on every connect —
-- indexed so this stays O(log n) rather than a full scan as the device count grows.
CREATE INDEX IF NOT EXISTS idx_devices_token_hash ON devices(token_hash);

-- The reap sweep and the "list active devices" panel both filter on this shape.
CREATE INDEX IF NOT EXISTS idx_devices_revoked_last_seen ON devices(revoked_at, last_seen_at);
