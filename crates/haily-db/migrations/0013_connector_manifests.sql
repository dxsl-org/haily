-- Human-approved connector manifests (Safe Operator Harness phase 4, R3 substrate).
-- A manifest declares one connector's base_url + a pinned IP/CIDR allowance (C3) + the
-- ops it exposes; the registry loads all `active` rows at startup and registers ONE
-- generic HttpConnectorTool per op (no codegen — the tool interprets the manifest).
--
-- Human-only to write (m3): NO Tool/LLM path inserts or activates a manifest — the only
-- writer is a human-invoked admin path (CLI/manual SQL). Immutable per version:
-- manifest_json/content_hash/version are append-only (trigger below); a new schema is a
-- NEW row (new version) requiring re-approval, never an in-place mutation of an approved
-- one. `status` (active<->disabled) STAYS mutable so a human can revoke a connector.
--
-- NO FK to/from action_journal: journal.tool_name is a denormalized string (0012 note),
-- and a manifest row must NOT participate in the kms_skills/decay path (kms invariant —
-- no confidence/EMA/archived_at columns here, and no query joins this table to skills).
CREATE TABLE IF NOT EXISTS connector_manifests (
    id                TEXT PRIMARY KEY,
    -- Logical connector name, e.g. "odoo". Ops are namespaced under it.
    connector_name    TEXT NOT NULL,
    -- Monotonic human-assigned version string. (name, version) is the approval unit.
    version           TEXT NOT NULL,
    -- Deterministic hash of manifest_json — pins the exact schema. A hash mismatch on
    -- reload means the stored JSON was tampered with out-of-band → re-approval required.
    content_hash      TEXT NOT NULL,
    -- The full manifest document (serde `Manifest`). Immutable per version.
    manifest_json     TEXT NOT NULL,
    -- Approved base URL. Its resolved IP/CIDR is pinned in allowed_ip_cidrs at approval
    -- time (C3) — never re-derived from the hostname at call time (DNS-rebind to IMDS).
    base_url          TEXT NOT NULL,
    -- JSON array of pinned IP/CIDR strings (C3). NOT hostnames. The SSRF allowance at
    -- call time permits a private addr ONLY if it matches one of these AND is not
    -- metadata/link-local (those are NEVER allowable, even if listed).
    allowed_ip_cidrs  TEXT NOT NULL,
    -- active | disabled. Only `active` manifests are loaded + registered at startup.
    status            TEXT NOT NULL DEFAULT 'active',
    created_at        TEXT NOT NULL,
    UNIQUE(connector_name, version)
);

CREATE INDEX IF NOT EXISTS idx_connector_manifests_status
    ON connector_manifests(status);

-- Append-only guard on the APPROVAL-BEARING columns only. manifest_json/content_hash/
-- version can never be rewritten in place — a changed schema is a new versioned row that
-- a human re-approves. `status` and `created_at` are deliberately EXCLUDED so a human can
-- toggle active<->disabled (revoke) without minting a new version.
CREATE TRIGGER IF NOT EXISTS connector_manifests_no_update
BEFORE UPDATE OF manifest_json, content_hash, version
ON connector_manifests
FOR EACH ROW
WHEN
    OLD.manifest_json IS NOT NEW.manifest_json
    OR OLD.content_hash IS NOT NEW.content_hash
    OR OLD.version IS NOT NEW.version
BEGIN
    SELECT RAISE(ABORT, 'connector_manifests: manifest_json/content_hash/version are immutable per version');
END;
