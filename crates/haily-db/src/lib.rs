pub mod queries;

use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;

#[derive(Clone)]
pub struct DbHandle {
    pub(crate) pool: SqlitePool,
}

impl DbHandle {
    pub async fn init(db_path: &Path) -> Result<Self> {
        let url = format!("sqlite://{}", db_path.display());
        let opts = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;

        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Best-effort WAL flush on graceful shutdown.
    ///
    /// `TRUNCATE` checkpoints all committed frames back into the main DB file and
    /// truncates the `-wal` file to zero bytes — avoids a lingering large WAL file
    /// across restarts. Not required for correctness: WAL mode is crash-safe by
    /// design and SQLite recovers an un-checkpointed WAL automatically on next open.
    ///
    /// Returns `true` when the checkpoint was BUSY — another connection still held a
    /// lock, so nothing was truncated. That is a silent no-op, not an error (sqlx
    /// reports the PRAGMA row, not a failure), so the caller can surface it rather than
    /// logging a misleading "complete". WAL mode stays crash-safe either way.
    pub async fn wal_checkpoint_truncate(&self) -> Result<bool> {
        // The PRAGMA returns one row: (busy, log, checkpointed).
        let row: Option<(i64, i64, i64)> = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(busy, _, _)| busy != 0).unwrap_or(false))
    }
}
