//! Backup starvation warning (M7a) — split out of `mod.rs` for file-size hygiene.
//!
//! `backup.age_warning_active` is a PERSISTED flag (mirrors
//! `credential_store::FALLBACK_WARNING_PREF`'s pattern in `haily-app`) the GUI reads on
//! open — a warn-only log line is how a silently-starved backup stays invisible forever.
//! A fresh install is given a grace period (measured from `backup.worker_first_seen_at`,
//! set on this worker's very first cycle) rather than flagging staleness before the
//! first backup has had a chance to run at all.
use super::gfs::list_tier_files;
use super::PREF_LAST_SUCCESS_AT;
use chrono::{DateTime, Utc};
use haily_db::{queries::meta, DbHandle};
use std::path::Path;
use tracing::warn;

const PREF_FIRST_SEEN_AT: &str = "backup.worker_first_seen_at";
const PREF_AGE_WARNING_ACTIVE: &str = "backup.age_warning_active";
const PREF_DIR_SIZE_BYTES: &str = "backup.dir_size_bytes";

/// A daily-cadence worker missing more than this many days straight has a real problem
/// (disk full, permissions, a crash loop) — one skipped day (e.g. the machine was off)
/// is not itself alarming.
const AGE_WARNING_THRESHOLD_DAYS: i64 = 2;

/// Reads (or, on first call ever, records) the timestamp staleness should be measured
/// from: the last successful backup if one exists, else this worker's first-observed
/// boot — never `None` treated as "infinitely stale", which would alarm a brand-new
/// install before its first daily cycle has even run.
async fn staleness_reference(db: &DbHandle, now: DateTime<Utc>) -> DateTime<Utc> {
    if let Ok(Some(s)) = meta::get_preference(db, PREF_LAST_SUCCESS_AT).await {
        if let Ok(ts) = DateTime::parse_from_rfc3339(&s) {
            return ts.with_timezone(&Utc);
        }
    }
    if let Ok(Some(s)) = meta::get_preference(db, PREF_FIRST_SEEN_AT).await {
        if let Ok(ts) = DateTime::parse_from_rfc3339(&s) {
            return ts.with_timezone(&Utc);
        }
    }
    if let Err(e) = meta::upsert_preference(db, PREF_FIRST_SEEN_AT, &now.to_rfc3339(), "backup_worker").await {
        warn!(error = %e, "backup: failed to persist worker first-seen timestamp");
    }
    now
}

/// Refreshes `backup.age_warning_active` and `backup.dir_size_bytes` (so a runaway DB
/// growing the backup directory unboundedly is visible alongside the warning, per the
/// phase's risk notes) every cycle — best-effort; a failure to persist either must not
/// crash the worker.
pub(super) async fn check_and_warn_on_stale_backup(db: &DbHandle, dir: &Path) {
    let now = Utc::now();
    let reference = staleness_reference(db, now).await;
    let is_stale = now.signed_duration_since(reference).num_days() > AGE_WARNING_THRESHOLD_DAYS;

    if let Err(e) =
        meta::upsert_preference(db, PREF_AGE_WARNING_ACTIVE, if is_stale { "true" } else { "false" }, "backup_worker")
            .await
    {
        warn!(error = %e, "backup: failed to persist age-warning flag");
    }

    let total_bytes: u64 = ["daily", "weekly", "monthly"]
        .iter()
        .flat_map(|tier| list_tier_files(dir, tier))
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    if let Err(e) = meta::upsert_preference(db, PREF_DIR_SIZE_BYTES, &total_bytes.to_string(), "backup_worker").await
    {
        warn!(error = %e, "backup: failed to persist backup-dir size");
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

    /// A fresh install (no backup yet) must NOT be flagged stale on its very first
    /// staleness check — only after the grace period has actually elapsed.
    #[tokio::test]
    async fn fresh_install_is_not_flagged_stale_immediately() {
        let (db, dir) = test_db().await;
        let backups_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();

        check_and_warn_on_stale_backup(&db, &backups_dir).await;

        let flag = meta::get_preference(&db, PREF_AGE_WARNING_ACTIVE).await.unwrap();
        assert_eq!(flag.as_deref(), Some("false"), "a brand-new install must not show the staleness warning yet");
    }

    /// Once the last successful backup is older than the threshold, the warning flag
    /// must flip to active — this is the user-visible surface for silent starvation.
    #[tokio::test]
    async fn stale_last_success_flips_the_warning_flag_active() {
        let (db, dir) = test_db().await;
        let backups_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();

        let stale_ts = Utc::now() - chrono::Duration::days(AGE_WARNING_THRESHOLD_DAYS + 1);
        meta::upsert_preference(&db, PREF_LAST_SUCCESS_AT, &stale_ts.to_rfc3339(), "test").await.unwrap();

        check_and_warn_on_stale_backup(&db, &backups_dir).await;

        let flag = meta::get_preference(&db, PREF_AGE_WARNING_ACTIVE).await.unwrap();
        assert_eq!(flag.as_deref(), Some("true"));
    }
}
