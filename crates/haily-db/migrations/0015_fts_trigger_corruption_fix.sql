-- Fix a pre-existing FTS5 external-content corruption bug in the `tasks_au`, `notes_au`,
-- and `kms_facts_au` AFTER-UPDATE triggers (migrations 0002/0007/0009). Discovered while
-- implementing the Safe Operator Harness local-tool undo (phase 1): undoing a task_complete
-- or task_delete does a SECOND UPDATE on the same row, which raises `SQLITE_CORRUPT`
-- ("database disk image is malformed") under the original trigger shape.
--
-- Root cause: each `*_au` trigger unconditionally issues an FTS5 `'delete'` command for
-- `old.rowid` before conditionally re-inserting. FTS5's external-content contract requires
-- a `'delete'` command's rowid to currently be present in the index (SQLite docs: deleting a
-- rowid/content combination that is not indexed is undefined behavior, up to and including
-- corrupting the shadow tables). The bug: once a row transitions OUT of the indexed set
-- (e.g. task -> status='done', note/fact -> deleted_at set), the FIRST update's trigger
-- deletes it from FTS. A SECOND update on that same row (e.g. undo restoring status='todo')
-- fires the trigger again, which tries to delete a rowid ALREADY ABSENT from the index —
-- exactly the corrupting case.
--
-- Fix: guard the `'delete'` half with the SAME predicate as the conditional insert, using
-- `old.*` columns — a delete is only safe (and only necessary) when the OLD row was ACTUALLY
-- indexed. When old and new are both indexed (e.g. an ordinary content edit), old.rowid ==
-- new.rowid so the delete-then-reinsert still correctly refreshes the FTS entry.
DROP TRIGGER IF EXISTS tasks_au;
CREATE TRIGGER tasks_au AFTER UPDATE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, title, description)
    SELECT 'delete', old.rowid, old.title, old.description
    WHERE old.deleted_at IS NULL AND old.status NOT IN ('done', 'cancelled');
    INSERT INTO tasks_fts(rowid, title, description)
    SELECT new.rowid, new.title, new.description
    WHERE new.deleted_at IS NULL AND new.status NOT IN ('done', 'cancelled');
END;

DROP TRIGGER IF EXISTS notes_au;
CREATE TRIGGER notes_au AFTER UPDATE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, content)
    SELECT 'delete', old.rowid, old.title, old.content
    WHERE old.deleted_at IS NULL;
    INSERT INTO notes_fts(rowid, title, content)
    SELECT new.rowid, new.title, new.content WHERE new.deleted_at IS NULL;
END;

DROP TRIGGER IF EXISTS kms_facts_au;
CREATE TRIGGER kms_facts_au AFTER UPDATE ON kms_facts BEGIN
    INSERT INTO facts_fts(facts_fts, rowid, subject, predicate, object)
    SELECT 'delete', old.rowid, old.subject, old.predicate, old.object
    WHERE old.deleted_at IS NULL AND old.archived_at IS NULL;
    INSERT INTO facts_fts(rowid, subject, predicate, object)
    SELECT new.rowid, new.subject, new.predicate, new.object
    WHERE new.deleted_at IS NULL AND new.archived_at IS NULL;
END;

-- Unconditional shadow-table rebuild: the buggy triggers above may have already corrupted
-- (or left inconsistent) the FTS index on any existing database before this migration ran.
-- 'rebuild' regenerates the FTS shadow tables from the authoritative content tables
-- (tasks/notes/kms_facts) and is always safe to run, even on an already-healthy index.
INSERT INTO tasks_fts(tasks_fts) VALUES ('rebuild');
INSERT INTO notes_fts(notes_fts) VALUES ('rebuild');
INSERT INTO facts_fts(facts_fts) VALUES ('rebuild');
