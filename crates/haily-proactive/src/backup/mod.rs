//! Scheduled GFS (grandfather-father-son) SQLite backup worker — Phase 6 "Activate &
//! Measure", closing blind-spot #1: the entire "life memory" lives in one `haily.db`
//! file with no backup.
//!
//! Three tiers (daily/weekly/monthly), each with its own **preference-configured**
//! retention (`backup.retention_{daily,weekly,monthly}`, defaults 7/4/6 — re-read every
//! cycle so a config change takes effect without a restart, see [`gfs`]). Exactly one
//! `VACUUM INTO` runs per calendar day (the daily snapshot); weekly/monthly are cheap
//! file-copy PROMOTIONS of that same snapshot, not extra `VACUUM INTO` calls — this
//! keeps the per-day I/O cost bounded to one whole-DB rewrite regardless of how many
//! tiers land that day (USER-DECIDED at validation 2026-07-06).
//!
//! ## Credential posture (M7a, revised M7b)
//! A backup taken while a connector credential still sits in plaintext
//! `kms_preferences` (not yet migrated into the OS keyring) must not retain that secret
//! in the copy. `credential_migration_clean` (computed once at boot in
//! `haily-app::bootstrap`, which alone has visibility into `CredentialStore`/keyring
//! state — this crate sits BELOW `haily-app` in the dependency graph and must not reach
//! up into it) used to GATE the first-ever scheduled backup outright: no backup at all
//! until migration was clean. That defeats durability — the headline guarantee this
//! worker exists for — whenever the keyring is PERSISTENTLY unavailable (e.g. a
//! non-headless boot with no working credential backend): migration then never
//! completes, `credential_migration_clean` is `false` on every single boot, and the
//! entire life-memory DB never gets backed up at all.
//!
//! M7b fix: the backup is now ALWAYS written. If migration is not clean this cycle, the
//! just-written copy is scrubbed of the plaintext credential preference row(s)
//! (`credential_scrub`) — a real `DELETE` + `VACUUM` on the COPY, never the live
//! database — before it is promoted or counted as successful. This satisfies both
//! invariants literally: life memory is never withheld, and no backup file ever
//! contains a plaintext credential. A scrub failure discards that cycle's copy (fails
//! closed) rather than risk shipping it; the next hourly cycle tries again.
//!
//! ## Starvation warning (M7a)
//! See [`staleness`] for the persisted, GUI-visible warning flag.
mod credential_scrub;
mod gfs;
mod staleness;

use chrono::{NaiveDate, Utc};
use haily_db::{queries::meta, DbHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Wake hourly and decide internally whether today's snapshot is still due — mirrors
/// `daily_rollup.rs`'s `CHECK_INTERVAL_SECS` pattern. Cancellation is observed at every
/// sleep point, never after a full 24h wait.
const CHECK_INTERVAL_SECS: u64 = 3600;

/// Shared with `staleness` (measures backup age from this) — kept here since `run_cycle`
/// below is also the writer.
const PREF_LAST_SUCCESS_AT: &str = "backup.last_success_at";

/// Runs until `shutdown` is cancelled: once per day, write a fresh daily snapshot,
/// promote it onto the weekly/monthly tiers if this is the first snapshot seen for that
/// bucket, prune each tier to its configured retention, and refresh the starvation
/// warning flag. `backups_dir` is created if missing on every cycle (cheap, idempotent).
pub async fn loop_forever(
    db: Arc<DbHandle>,
    backups_dir: PathBuf,
    credential_migration_clean: bool,
    credential_preference_keys: Vec<String>,
    shutdown: CancellationToken,
) {
    loop {
        match std::fs::create_dir_all(&backups_dir) {
            Ok(()) => {
                run_cycle(&db, &backups_dir, credential_migration_clean, &credential_preference_keys).await
            }
            Err(e) => warn!(
                error = %e,
                dir = %backups_dir.display(),
                "backup: could not create backups directory — skipping this cycle"
            ),
        }
        staleness::check_and_warn_on_stale_backup(&db, &backups_dir).await;

        tokio::select! {
            _ = shutdown.cancelled() => { info!("backup loop shutting down"); break; }
            _ = tokio::time::sleep(std::time::Duration::from_secs(CHECK_INTERVAL_SECS)) => {}
        }
    }
}

/// One cycle's work: at most one `VACUUM INTO` per calendar day (idempotent on the
/// daily filename's existence), followed by an optional credential scrub, then GFS
/// promotion and per-tier pruning.
async fn run_cycle(db: &DbHandle, dir: &Path, credential_migration_clean: bool, credential_preference_keys: &[String]) {
    let today = Utc::now().date_naive();
    let daily_path = dir.join(gfs::daily_filename(today));

    if daily_path.exists() {
        return; // Already backed up today.
    }

    match db.backup_to(&daily_path).await {
        Ok(()) => {
            if !credential_migration_clean {
                if let Err(e) =
                    credential_scrub::scrub_credential_preferences(&daily_path, credential_preference_keys).await
                {
                    // Fail closed: a copy we could not confirm is scrubbed must never be
                    // promoted or counted as a success. Remove it and retry next cycle —
                    // `daily_path.exists()` above is the only gate on re-attempting, so
                    // this costs at most one hour of staleness, never durability.
                    warn!(
                        error = %e,
                        path = %daily_path.display(),
                        "backup: credential scrub failed on a not-yet-migrated copy — discarding this backup \
                         rather than risk shipping a plaintext credential"
                    );
                    let _ = std::fs::remove_file(&daily_path);
                    return;
                }
                warn!(
                    "backup: connector credential migration is not clean yet — wrote today's backup with the \
                     plaintext credential preference scrubbed from the COPY only (live storage is untouched); \
                     re-provision the connector credential after restoring from this snapshot"
                );
            }

            info!(path = %daily_path.display(), "daily backup written");
            if let Err(e) =
                meta::upsert_preference(db, PREF_LAST_SUCCESS_AT, &Utc::now().to_rfc3339(), "backup_worker").await
            {
                warn!(error = %e, "backup: failed to persist last-success timestamp");
            }
            promote_and_prune(db, dir, today, &daily_path).await;
        }
        Err(e) => warn!(error = %e, "daily backup failed"),
    }
}

/// Copies today's already-written daily snapshot onto the weekly/monthly tiers if no
/// snapshot exists yet for that bucket (existence-based, not weekday-based — self-heals
/// if the daily cadence missed the actual first day of a week/month, e.g. the machine
/// was asleep), then prunes every tier to its current preference-configured retention.
async fn promote_and_prune(db: &DbHandle, dir: &Path, today: NaiveDate, daily_path: &Path) {
    let weekly_path = dir.join(gfs::weekly_filename(today));
    if !weekly_path.exists() {
        match std::fs::copy(daily_path, &weekly_path) {
            Ok(_) => info!(path = %weekly_path.display(), "weekly backup promoted"),
            Err(e) => warn!(error = %e, "backup: weekly GFS promotion failed"),
        }
    }

    let monthly_path = dir.join(gfs::monthly_filename(today));
    if !monthly_path.exists() {
        match std::fs::copy(daily_path, &monthly_path) {
            Ok(_) => info!(path = %monthly_path.display(), "monthly backup promoted"),
            Err(e) => warn!(error = %e, "backup: monthly GFS promotion failed"),
        }
    }

    gfs::prune_tier(db, dir, "daily", gfs::PREF_RETENTION_DAILY, gfs::DEFAULT_RETENTION_DAILY).await;
    gfs::prune_tier(db, dir, "weekly", gfs::PREF_RETENTION_WEEKLY, gfs::DEFAULT_RETENTION_WEEKLY).await;
    gfs::prune_tier(db, dir, "monthly", gfs::PREF_RETENTION_MONTHLY, gfs::DEFAULT_RETENTION_MONTHLY).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> (Arc<DbHandle>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(DbHandle::init(&dir.path().join("haily.db")).await.expect("db init"));
        (db, dir)
    }

    /// Cancellation must end the loop promptly, mirroring every other interval worker.
    #[tokio::test]
    async fn loop_exits_promptly_on_cancel() {
        let (db, dir) = test_db().await;
        let backups_dir = dir.path().join("backups");
        let shutdown = CancellationToken::new();

        let handle = tokio::spawn(loop_forever(db, backups_dir, true, vec![], shutdown.clone()));
        shutdown.cancel();

        tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("loop must exit promptly on cancellation")
            .expect("task must not panic");
    }

    /// A full cycle must write all three GFS tiers on the very first run (each tier's
    /// promotion is existence-based, so day one always promotes to weekly + monthly too).
    #[tokio::test]
    async fn first_cycle_writes_all_three_gfs_tiers() {
        let (db, dir) = test_db().await;
        let backups_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();

        let today = Utc::now().date_naive();
        run_cycle(&db, &backups_dir, true, &[]).await;

        assert!(backups_dir.join(gfs::daily_filename(today)).exists(), "daily tier missing");
        assert!(backups_dir.join(gfs::weekly_filename(today)).exists(), "weekly tier missing");
        assert!(backups_dir.join(gfs::monthly_filename(today)).exists(), "monthly tier missing");
    }

    /// Running the cycle twice on the same day must not write a second daily snapshot
    /// (daily cadence — at most one `VACUUM INTO` per calendar day).
    #[tokio::test]
    async fn second_cycle_same_day_is_a_no_op() {
        let (db, dir) = test_db().await;
        let backups_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();

        run_cycle(&db, &backups_dir, true, &[]).await;
        let today = Utc::now().date_naive();
        let daily_path = backups_dir.join(gfs::daily_filename(today));
        let first_modified = std::fs::metadata(&daily_path).unwrap().modified().unwrap();

        // A second call on the same calendar day must be a pure no-op (path already
        // exists → early return), not attempt to overwrite it again.
        run_cycle(&db, &backups_dir, true, &[]).await;
        let second_modified = std::fs::metadata(&daily_path).unwrap().modified().unwrap();
        assert_eq!(first_modified, second_modified, "same-day re-run must not rewrite the daily snapshot");
    }

    /// M7b: a keyring that is PERSISTENTLY unavailable (so `credential_migration_clean`
    /// is `false` on every single boot, forever) must never withhold the backup — life
    /// memory durability is not optional. The backup IS written, and the plaintext
    /// credential preference is scrubbed from the copy (not the live DB) so invariant 2
    /// (no plaintext ships) still holds without sacrificing invariant 1 (never withheld).
    #[tokio::test]
    async fn backup_still_happens_when_credential_migration_is_permanently_not_clean_but_scrubs_plaintext() {
        let (db, dir) = test_db().await;
        let backups_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();

        meta::upsert_preference(&db, "connector.odoo.api_key", "sk-plaintext-secret", "test").await.unwrap();

        let credential_keys = vec!["connector.odoo.api_key".to_string()];
        run_cycle(&db, &backups_dir, false, &credential_keys).await;

        let today = Utc::now().date_naive();
        let daily_path = backups_dir.join(gfs::daily_filename(today));
        assert!(daily_path.exists(), "backup must be written even while credential migration is never clean");

        let copy = DbHandle::init(&daily_path).await.expect("open backup copy");
        let scrubbed = meta::get_preference(&copy, "connector.odoo.api_key").await.expect("read copy");
        assert!(scrubbed.is_none(), "plaintext credential must be scrubbed from the backup copy");

        let live = meta::get_preference(&db, "connector.odoo.api_key").await.expect("read live db");
        assert_eq!(live.as_deref(), Some("sk-plaintext-secret"), "the scrub must never touch the live database");
    }

    /// When migration IS clean, the scrub path must not run at all — a credential
    /// preference (e.g. already holding the keyring marker) must survive untouched in
    /// the backup copy, confirming the scrub is conditional, not unconditional cleanup.
    #[tokio::test]
    async fn credential_preference_is_preserved_in_backup_when_migration_is_clean() {
        let (db, dir) = test_db().await;
        let backups_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();

        meta::upsert_preference(&db, "connector.odoo.api_key", "haily-keyring-ref:connector.odoo.api_key", "test")
            .await
            .unwrap();

        let credential_keys = vec!["connector.odoo.api_key".to_string()];
        run_cycle(&db, &backups_dir, true, &credential_keys).await;

        let today = Utc::now().date_naive();
        let daily_path = backups_dir.join(gfs::daily_filename(today));
        let copy = DbHandle::init(&daily_path).await.expect("open backup copy");
        let preserved = meta::get_preference(&copy, "connector.odoo.api_key").await.expect("read copy");
        assert!(preserved.is_some(), "a clean migration state must not trigger scrubbing");
    }
}
