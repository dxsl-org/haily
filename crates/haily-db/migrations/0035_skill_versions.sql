-- Skill editor version history (Unified Chat UI phase 8, D4): ONE mechanism covering both
-- authored (kit-pack file) and synthesized (kms_skills row) edits, so revert/promote/crash-
-- recovery share a single table instead of two parallel histories. `content_md` for an
-- authored row is the skill's FULL file bytes (frontmatter + body), so a revert can write it
-- back verbatim; for a synthesized row it is the rendered 4-section body that replaced
-- `kms_skills.description`.
--
-- Append-only BY CONVENTION (no UPDATE/DELETE query is ever written against this table) — an
-- audit trail of every pre-edit snapshot, not an evidentiary ledger like `action_journal`, so
-- no immutability trigger is added here (YAGNI: nothing in this phase needs to survive a
-- malicious/buggy caller issuing a raw UPDATE, unlike `action_journal`'s undo-status column).
CREATE TABLE IF NOT EXISTS skill_versions (
    id         TEXT PRIMARY KEY,
    skill_name TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('authored', 'synthesized')),
    content_md TEXT NOT NULL,
    sha256     TEXT NOT NULL,
    note       TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_skill_versions_name ON skill_versions(skill_name, created_at DESC);
