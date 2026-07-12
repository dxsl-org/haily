-- Coding pipeline workspaces (Sub-Agent + Skill Architecture phase 1). One row per
-- CodingWorkspace: a dedicated git worktree of a target repo, bound to the session (and,
-- optionally, a work_item) that requested it. The worktree is the SINGLE authoritative
-- compensator for in-workspace file changes — undo of any coding change is a worktree
-- reset (`git checkout -- . && git clean -ffdx`), NOT a DB-row restore. This table is
-- therefore audit/lifecycle metadata only; it does not store file content or diffs.
--
-- soft-delete via `deleted_at` mirrors work_items: a discarded workspace stays as an
-- evidentiary row (which repo/branch/worktree existed) rather than being hard-removed, so
-- the orphan-worktree GC (P4) can reconcile filesystem worktrees against non-terminal rows.
CREATE TABLE IF NOT EXISTS coding_workspaces (
    id             TEXT PRIMARY KEY,
    session_id     TEXT NOT NULL REFERENCES sessions(id),
    -- Absolute path of the TARGET repo the worktree was cut from (the real repo).
    repo_path      TEXT NOT NULL,
    -- Branch the worktree checked out (workspace-local; never the target's main branch).
    branch         TEXT NOT NULL,
    -- Absolute path of the ephemeral worktree itself (under the app data dir).
    worktree_path  TEXT NOT NULL,
    -- Optional owning work_item; NULL for an ad-hoc workspace not tied to a tracked item.
    work_item_id   TEXT REFERENCES work_items(id),
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    deleted_at     TEXT
);

CREATE INDEX IF NOT EXISTS idx_coding_workspaces_session
    ON coding_workspaces(session_id, created_at);

CREATE INDEX IF NOT EXISTS idx_coding_workspaces_active
    ON coding_workspaces(deleted_at);
