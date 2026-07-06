//! GFS (grandfather-father-son) filename convention + per-tier retention pruning.
//! Split out of `mod.rs` for file-size hygiene — these are pure/filesystem helpers with
//! no relation to the daily-cadence orchestration in the parent module.
use chrono::{Datelike, NaiveDate};
use haily_db::{queries::meta, DbHandle};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub(super) const PREF_RETENTION_DAILY: &str = "backup.retention_daily";
pub(super) const PREF_RETENTION_WEEKLY: &str = "backup.retention_weekly";
pub(super) const PREF_RETENTION_MONTHLY: &str = "backup.retention_monthly";

pub(super) const DEFAULT_RETENTION_DAILY: usize = 7;
pub(super) const DEFAULT_RETENTION_WEEKLY: usize = 4;
pub(super) const DEFAULT_RETENTION_MONTHLY: usize = 6;

pub(super) fn daily_filename(date: NaiveDate) -> String {
    format!("haily-daily-{}.db", date.format("%Y%m%d"))
}

pub(super) fn weekly_filename(date: NaiveDate) -> String {
    let iso = date.iso_week();
    format!("haily-weekly-{:04}{:02}.db", iso.year(), iso.week())
}

pub(super) fn monthly_filename(date: NaiveDate) -> String {
    format!("haily-monthly-{}.db", date.format("%Y%m"))
}

async fn read_retention(db: &DbHandle, pref_key: &str, default: usize) -> usize {
    match meta::get_preference(db, pref_key).await {
        Ok(Some(v)) => v.parse::<usize>().unwrap_or(default),
        _ => default,
    }
}

/// Every file in `dir` whose name matches `haily-{tier}-*.db` — never touches a file
/// that does not match one of the three known tier patterns (manual files, unrelated
/// data), per the phase's retention-prune robustness requirement.
pub(super) fn list_tier_files(dir: &Path, tier: &str) -> Vec<PathBuf> {
    let prefix = format!("haily-{tier}-");
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".db"))
            })
            .collect(),
        Err(e) => {
            warn!(error = %e, dir = %dir.display(), "backup: could not list backups directory");
            Vec::new()
        }
    }
}

/// Keeps the `retention` most recent files matching `haily-{tier}-*.db` in `dir`,
/// deleting the rest. Filenames are zero-padded date strings (`YYYYMMDD`/`YYYYWW`/
/// `YYYYMM`), so lexicographic order is chronological order — no need to parse dates.
pub(super) async fn prune_tier(db: &DbHandle, dir: &Path, tier: &str, pref_key: &str, default: usize) {
    let retention = read_retention(db, pref_key, default).await;
    let mut matches = list_tier_files(dir, tier);
    if matches.len() <= retention {
        return;
    }
    matches.sort();
    let to_remove = matches.len() - retention;
    for path in matches.into_iter().take(to_remove) {
        match std::fs::remove_file(&path) {
            Ok(()) => info!(path = %path.display(), tier, "pruned old backup"),
            Err(e) => warn!(error = %e, path = %path.display(), tier, "backup: failed to prune old backup"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::queries::meta;

    async fn test_db() -> (DbHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = DbHandle::init(&dir.path().join("haily.db")).await.expect("db init");
        (db, dir)
    }

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    #[test]
    fn filenames_follow_the_locked_tier_naming_convention() {
        let d = ymd(2026, 7, 6); // Monday, ISO week 28
        assert_eq!(daily_filename(d), "haily-daily-20260706.db");
        assert_eq!(weekly_filename(d), "haily-weekly-202628.db");
        assert_eq!(monthly_filename(d), "haily-monthly-202607.db");
    }

    /// Per-tier prune must respect a preference-configured retention count, and changing
    /// the preference must take effect on the very next prune call — no restart needed.
    #[tokio::test]
    async fn prune_tier_respects_configurable_retention_without_restart() {
        let (db, dir) = test_db().await;
        let backups_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();

        for i in 1..=5 {
            std::fs::write(backups_dir.join(format!("haily-daily-2026070{i}.db")), b"x").unwrap();
        }

        // Default retention (7) keeps all 5.
        prune_tier(&db, &backups_dir, "daily", PREF_RETENTION_DAILY, DEFAULT_RETENTION_DAILY).await;
        assert_eq!(list_tier_files(&backups_dir, "daily").len(), 5);

        // Lower the preference to 2 — must prune down to the 2 most recent on the very
        // next call, proving the config change applies without a restart.
        meta::upsert_preference(&db, PREF_RETENTION_DAILY, "2", "test").await.unwrap();
        prune_tier(&db, &backups_dir, "daily", PREF_RETENTION_DAILY, DEFAULT_RETENTION_DAILY).await;
        let remaining = list_tier_files(&backups_dir, "daily");
        assert_eq!(remaining.len(), 2);
        let mut names: Vec<String> =
            remaining.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["haily-daily-20260704.db", "haily-daily-20260705.db"]);
    }

    /// Pruning one tier must never delete a file belonging to another tier or an
    /// unrelated manual file sitting in the same directory.
    #[tokio::test]
    async fn prune_never_touches_other_tiers_or_unrelated_files() {
        let (db, dir) = test_db().await;
        let backups_dir = dir.path().join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();

        std::fs::write(backups_dir.join("haily-daily-20260701.db"), b"x").unwrap();
        std::fs::write(backups_dir.join("haily-weekly-202627.db"), b"x").unwrap();
        std::fs::write(backups_dir.join("my-manual-copy.db"), b"x").unwrap();

        meta::upsert_preference(&db, PREF_RETENTION_DAILY, "0", "test").await.unwrap();
        prune_tier(&db, &backups_dir, "daily", PREF_RETENTION_DAILY, DEFAULT_RETENTION_DAILY).await;

        assert!(!backups_dir.join("haily-daily-20260701.db").exists());
        assert!(backups_dir.join("haily-weekly-202627.db").exists(), "weekly tier must survive a daily prune");
        assert!(backups_dir.join("my-manual-copy.db").exists(), "unrelated file must never be touched");
    }
}
