pub mod calendar;
pub mod memory;
pub mod notes;
pub mod reminders;
pub mod tasks;
pub mod web;
pub mod work_items;
pub mod worktree_tool;

/// M4 out-param write helper shared by every local `ReversibleWrite` tool
/// (tasks/notes/reminders): sets `ctx.last_journal_id` from a `local_journaled_write`
/// outcome, AFTER that call has already committed its transaction with
/// `post_state_version` recorded (see `local_journaled_write`'s doc comment) — so a
/// `Some` write here always implies the C10 undo-guard's baseline version landed. A
/// `None` outcome (target not found — mutation rolled back, nothing journaled) clears
/// the cell rather than leaving a stale id from... nothing, since it was never set for
/// this call in the first place; the explicit clear guards against a future caller
/// reusing one `ToolContext`/cell across more than one write in the same `execute()`.
pub(crate) fn set_last_journal_id(
    ctx: &crate::ToolContext,
    outcome: Option<&(haily_db::queries::journal::ActionJournalRow, String)>,
) {
    let journal_id = outcome.map(|(row, _post_state_version)| row.id.clone());
    match ctx.last_journal_id.lock() {
        Ok(mut guard) => *guard = journal_id,
        Err(poisoned) => *poisoned.into_inner() = journal_id,
    }
}
