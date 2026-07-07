//! Reopens a standalone backup COPY file (produced by [`DbHandle::backup_to`], never
//! the live database) so a caller can run a follow-up maintenance write against it —
//! currently only the scheduled backup worker's credential scrub
//! (`haily-proactive::backup::credential_scrub`, M7b) does this.
use crate::{DbHandle, BUSY_TIMEOUT};
use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Opens `copy_path` with the DEFAULT rollback-journal mode (`DELETE`), deliberately
/// NOT the WAL mode `DbHandle::init` always requests for the live, long-running
/// database. A WAL-mode connection here would split the copy across a `.db` file and
/// an `-wal` sidecar; the backup worker's weekly/monthly promotion is a plain
/// `std::fs::copy` of just the `.db` file, so any maintenance write left sitting in an
/// uncheckpointed WAL — including a credential scrub's `DELETE` + `VACUUM` — would
/// silently vanish from those promoted copies. Rollback-journal mode keeps every write
/// inside the single `.db` file, which is what makes the daily snapshot promotable by a
/// plain file copy in the first place.
///
/// Deliberately does not run `sqlx::migrate!`: the copy already carries the exact
/// schema of its source at the moment `backup_to` ran, and re-running migrations here
/// is both redundant and, on a copy taken mid-upgrade, could apply a migration the
/// source database itself had not (yet) committed to.
///
/// # Errors
/// Returns an error if `copy_path` cannot be opened as a SQLite database (e.g. it does
/// not exist).
pub async fn open_standalone_copy_for_maintenance(copy_path: &Path) -> Result<DbHandle> {
    let url = format!("sqlite://{}", copy_path.display());
    let opts = SqliteConnectOptions::from_str(&url)?
        .journal_mode(SqliteJournalMode::Delete)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening backup copy for maintenance: {}", copy_path.display()))?;
    Ok(DbHandle { pool, maintenance_lock: Arc::new(Mutex::new(())) })
}
