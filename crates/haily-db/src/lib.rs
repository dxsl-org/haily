pub mod queries;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// How long a connection blocks waiting for a lock held by another connection before
/// giving up with `SQLITE_BUSY` (M7a). Without this, any writer racing a whole-DB
/// maintenance op (VACUUM/VACUUM INTO) that briefly holds an exclusive lock fails
/// immediately instead of just waiting the lock out — the maintenance lock below
/// serializes Haily's OWN maintenance ops against each other, but `busy_timeout` is
/// still needed for ordinary read/write connections that might overlap one.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct DbHandle {
    pub(crate) pool: SqlitePool,
    /// Serializes every WHOLE-DATABASE maintenance statement (`VACUUM`, `VACUUM INTO`)
    /// across every `DbHandle` clone (M7a). These statements briefly hold an exclusive
    /// lock on the whole file; running two concurrently (e.g. the daily-rollup VACUUM
    /// racing a scheduled backup, or the credential-migration scrub racing either) does
    /// not corrupt data — `busy_timeout` above absorbs the resulting contention — but it
    /// does mean one of them silently retries/blocks for the full timeout, which the
    /// M7a design explicitly wants to avoid ("collide" in the phase-6 requirements).
    /// Held for the duration of the statement, never across an `.await` boundary that
    /// waits on caller-supplied work — see `vacuum()`/`backup_to()`.
    maintenance_lock: Arc<Mutex<()>>,
}

impl DbHandle {
    pub async fn init(db_path: &Path) -> Result<Self> {
        let url = format!("sqlite://{}", db_path.display());
        let opts = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;

        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(Self { pool, maintenance_lock: Arc::new(Mutex::new(())) })
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

    /// Reclaim space freed by deletes (e.g. the daily rollup's raw-trace pruning,
    /// `queries::skills::delete_traces_older_than`) by rewriting the whole DB file.
    /// Exposed here (not as a bare query in `queries/`) since `VACUUM` is a
    /// whole-database maintenance operation, not a domain-scoped query — kept on
    /// `DbHandle` alongside `wal_checkpoint_truncate` for the same reason. Callers
    /// outside this crate (e.g. `haily-proactive`'s daily rollup worker) reach this
    /// instead of depending on `sqlx` directly, preserving "SQL only in
    /// `haily-db`" (CLAUDE.md).
    ///
    /// # Errors
    /// Returns an error if the `VACUUM` statement fails (e.g. another connection
    /// holds an exclusive lock).
    pub async fn vacuum(&self) -> Result<()> {
        let _guard = self.maintenance_lock.lock().await;
        sqlx::query("VACUUM").execute(&self.pool).await?;
        Ok(())
    }

    /// Write a fully-consistent, standalone copy of the database to `path` via
    /// `VACUUM INTO` (Phase 6 "Activate & Measure" — scheduled backup + manual export).
    /// WAL-safe: `VACUUM INTO` reads a transactional snapshot, so an uncheckpointed WAL
    /// is folded into the copy correctly without a separate checkpoint step and without
    /// blocking concurrent readers — chosen over the sqlite3 C backup API (not exposed
    /// through sqlx) and over a raw file copy (unsafe against a live WAL).
    ///
    /// Acquires the same whole-DB maintenance lock as `vacuum()` (M7a) so a scheduled
    /// backup can never run concurrently with the daily-rollup `VACUUM` or the
    /// credential-migration scrub — they serialize instead of colliding.
    ///
    /// Any file already at `path` is removed first: a repeat call (e.g. a worker
    /// re-running after a restart mid-cycle) overwrites the previous attempt rather than
    /// erroring on "file already exists", which `VACUUM INTO` itself refuses to do.
    ///
    /// # Errors
    /// Returns an error if a stale file at `path` cannot be removed, if `path` is not
    /// valid UTF-8, or if the `VACUUM INTO` statement fails (e.g. the parent directory
    /// does not exist).
    pub async fn backup_to(&self, path: &Path) -> Result<()> {
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale backup file at {}", path.display()))?;
        }
        let dest = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("backup path is not valid UTF-8: {}", path.display()))?;

        let _guard = self.maintenance_lock.lock().await;
        sqlx::query("VACUUM INTO ?").bind(dest).execute(&self.pool).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> (DbHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = DbHandle::init(&dir.path().join("haily.db")).await.expect("db init");
        (db, dir)
    }

    /// `backup_to` must produce a standalone file that opens on its own and contains
    /// the same rows as the source — the durability guarantee this phase exists for.
    #[tokio::test]
    async fn backup_to_produces_an_openable_copy_with_matching_row_counts() {
        let (db, dir) = test_db().await;
        sqlx::query(
            "INSERT INTO kms_preferences (id, key, value, confidence, source, created_at, updated_at)
             VALUES ('t1', 'k1', 'v1', 1.0, 'test', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&db.pool)
        .await
        .expect("seed row");

        let backup_path = dir.path().join("backup.db");
        db.backup_to(&backup_path).await.expect("backup_to");

        let copy = DbHandle::init(&backup_path).await.expect("open backup copy");
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM kms_preferences")
            .fetch_one(copy.pool())
            .await
            .expect("count rows in copy");
        assert_eq!(count, 1, "backup copy must contain the same rows as the source");
    }

    /// A repeat `backup_to` call against the same path (e.g. a restarted worker retrying
    /// the same day's snapshot) must overwrite, not fail on "file already exists".
    #[tokio::test]
    async fn backup_to_overwrites_an_existing_file_at_the_same_path() {
        let (db, dir) = test_db().await;
        let backup_path = dir.path().join("backup.db");
        db.backup_to(&backup_path).await.expect("first backup_to");
        db.backup_to(&backup_path).await.expect("second backup_to must overwrite, not error");
    }

    /// The maintenance lock (M7a) must make a concurrent `vacuum()` + `backup_to()`
    /// serialize rather than one erroring out from lock contention — both must succeed.
    #[tokio::test]
    async fn concurrent_vacuum_and_backup_serialize_without_erroring() {
        let (db, dir) = test_db().await;
        let backup_path = dir.path().join("backup.db");

        let db2 = db.clone();
        let vacuum_fut = tokio::spawn(async move { db2.vacuum().await });
        let backup_fut = tokio::spawn(async move { db.backup_to(&backup_path).await });

        let (vacuum_res, backup_res) = tokio::join!(vacuum_fut, backup_fut);
        vacuum_res.expect("vacuum task must not panic").expect("vacuum must succeed");
        backup_res.expect("backup task must not panic").expect("backup_to must succeed");
    }
}
