/// Harness Completion phase 5: daily aggregation of `kms_task_traces` into
/// `kms_daily_rollup`, followed by raw-row retention pruning and a periodic
/// `VACUUM` — researcher-03 §3's "collapse >90-day raw rows into the rollup, delete
/// raw, periodic VACUUM" recommendation, run alongside the existing hourly-synthesis/
/// daily-decay workers already in this crate's sibling modules.
use chrono::{Duration, Utc};
use haily_db::{queries::skills as db_skills, DbHandle};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Raw `kms_task_traces` rows older than this are eligible for rollup + deletion —
/// mirrors the 90-day precedent already used elsewhere in this codebase's
/// anti-reinforcement/fact-decay design (researcher-03 §3).
const RAW_RETENTION_DAYS: i64 = 90;

/// How often this loop wakes to check whether a new day's rollup is due. A daily job
/// does not need finer granularity than this; cancellation is still observed at every
/// sleep point so shutdown never waits out the full interval.
const CHECK_INTERVAL_SECS: u64 = 3600;

/// `VACUUM` reclaims space freed by `delete_traces_older_than` — run far less often
/// than the rollup itself (it rewrites the whole DB file) to keep its I/O cost
/// bounded; every 7th successful rollup cycle is a simple, dependency-free cadence
/// that needs no extra persisted state.
const VACUUM_EVERY_N_ROLLUPS: u64 = 7;

/// Runs until `shutdown` is cancelled: once per `CHECK_INTERVAL_SECS`, roll up
/// yesterday's (and any older, still-unrolled) raw traces, prune rows past
/// `RAW_RETENTION_DAYS`, and periodically `VACUUM`.
///
/// Rollup targets "yesterday" (`Utc::now() - 1 day`, not "today") so a partial day's
/// traces are never aggregated before the day has actually finished — running this
/// hourly and re-computing the same date is intentional and safe: `compute_daily_rollup`
/// upserts per `(date, model_tier)`, so a rerun replaces rather than double-counts.
pub async fn loop_forever(db: Arc<DbHandle>, shutdown: CancellationToken) {
    let mut rollup_cycles: u64 = 0;

    loop {
        let target_date = (Utc::now() - Duration::days(1)).format("%Y-%m-%d").to_string();

        match db_skills::compute_daily_rollup(&db, &target_date).await {
            Ok(n) => {
                if n > 0 {
                    info!(date = %target_date, tiers = n, "daily rollup computed");
                }
            }
            Err(e) => warn!(date = %target_date, error = %e, "daily rollup failed"),
        }

        match db_skills::delete_traces_older_than(&db, RAW_RETENTION_DAYS).await {
            Ok(deleted) if deleted > 0 => {
                info!(deleted, retention_days = RAW_RETENTION_DAYS, "raw task traces pruned past retention")
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "raw task trace pruning failed"),
        }

        rollup_cycles += 1;
        if rollup_cycles.is_multiple_of(VACUUM_EVERY_N_ROLLUPS) {
            if let Err(e) = db.vacuum().await {
                warn!(error = %e, "periodic VACUUM failed");
            } else {
                info!("periodic VACUUM completed");
            }
        }

        tokio::select! {
            _ = shutdown.cancelled() => { info!("daily rollup loop shutting down"); break; }
            _ = tokio::time::sleep(std::time::Duration::from_secs(CHECK_INTERVAL_SECS)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> (Arc<DbHandle>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("haily.db");
        let db = Arc::new(DbHandle::init(&db_path).await.expect("db init"));
        (db, dir)
    }

    /// Cancellation must end the loop promptly rather than waiting out the full
    /// `CHECK_INTERVAL_SECS` sleep — mirrors the shutdown-promptness proof every
    /// other interval worker in this crate already carries.
    #[tokio::test]
    async fn loop_exits_promptly_on_cancel() {
        let (db, _dir) = test_db().await;
        let shutdown = CancellationToken::new();

        let handle = tokio::spawn(loop_forever(db, shutdown.clone()));
        shutdown.cancel();

        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("loop must exit promptly on cancellation")
            .expect("task must not panic");
    }

    /// One full cycle (rollup + prune, cancelled before the sleep) must not error out
    /// even against a completely empty `kms_task_traces` table — the common case for
    /// a fresh install with no traces yet.
    #[tokio::test]
    async fn one_cycle_against_empty_traces_does_not_panic_or_hang() {
        let (db, _dir) = test_db().await;
        let shutdown = CancellationToken::new();
        // Pre-cancel: the loop still runs ONE full rollup+prune pass before observing
        // cancellation at the `select!` (cancellation is checked at the sleep point,
        // not before the body), so this still exercises the real work against an
        // empty table.
        shutdown.cancel();

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            loop_forever(db, shutdown),
        )
        .await
        .expect("a single pre-cancelled cycle must complete promptly");
    }
}
