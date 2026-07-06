/// Harness Completion phase 5: daily aggregation of `kms_task_traces` into
/// `kms_daily_rollup`, followed by raw-row retention pruning and a periodic
/// `VACUUM` — researcher-03 §3's "collapse >90-day raw rows into the rollup, delete
/// raw, periodic VACUUM" recommendation, run alongside the existing hourly-synthesis/
/// daily-decay workers already in this crate's sibling modules.
use chrono::{Duration, NaiveDate, Utc};
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

/// Computes the inclusive range of dates to roll on this cycle (M7b — Phase 8,
/// "Activate & Measure"). Bounded below by `RAW_RETENTION_DAYS` so a date whose raw
/// `kms_task_traces` rows are already gone (or about to be, via this same cycle's
/// `delete_traces_older_than` call below) is never targeted — rolling it would
/// silently write an empty/zero-count row that looks like "nothing happened that
/// day" rather than "we can no longer know."
///
/// `last_rolled`: the most recent date already present in `kms_daily_rollup`
/// (`None` on a fresh install). `today`: `Utc::now()`'s date, used only to derive
/// "yesterday" (the last fully-elapsed day — a partial "today" is never rolled) and
/// the retention floor.
///
/// Before this fix the loop only ever targeted `today - 1 day`: a laptop asleep
/// through an entire day (or the worker crash-looping) meant that day's traces were
/// pruned by `delete_traces_older_than` 90 days later having NEVER been rolled up —
/// permanently lost from the accrual thesis this phase's `golden-journeys.md` sprint
/// depends on. Resuming from `last_rolled + 1` closes that gap; `compute_daily_rollup`
/// upserts per `(date, model_tier)`, so re-targeting an already-rolled date here is
/// still safe (replaces, never double-counts).
fn dates_to_roll(last_rolled: Option<NaiveDate>, today: NaiveDate, retention_days: i64) -> Vec<NaiveDate> {
    let yesterday = today - Duration::days(1);
    let retention_floor = today - Duration::days(retention_days);
    let start = match last_rolled {
        Some(d) => std::cmp::max(d + Duration::days(1), retention_floor),
        None => retention_floor,
    };
    if start > yesterday {
        return Vec::new();
    }
    let mut dates = Vec::with_capacity((yesterday - start).num_days() as usize + 1);
    let mut cur = start;
    while cur <= yesterday {
        dates.push(cur);
        cur += Duration::days(1);
    }
    dates
}

/// Runs until `shutdown` is cancelled: once per `CHECK_INTERVAL_SECS`, roll up every
/// still-unrolled date from the last rollup through yesterday (M7b — see
/// `dates_to_roll`'s doc comment), prune raw rows past `RAW_RETENTION_DAYS`, and
/// periodically `VACUUM`.
pub async fn loop_forever(db: Arc<DbHandle>, shutdown: CancellationToken) {
    let mut rollup_cycles: u64 = 0;

    loop {
        let today = Utc::now().date_naive();
        let last_rolled = db_skills::latest_rollup_date(&db)
            .await
            .unwrap_or(None)
            .and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());

        for target_date in dates_to_roll(last_rolled, today, RAW_RETENTION_DAYS) {
            let target_date = target_date.format("%Y-%m-%d").to_string();
            match db_skills::compute_daily_rollup(&db, &target_date).await {
                Ok(n) => {
                    if n > 0 {
                        info!(date = %target_date, tiers = n, "daily rollup computed");
                    }
                }
                Err(e) => warn!(date = %target_date, error = %e, "daily rollup failed"),
            }
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

    // -- M7b: `dates_to_roll` pure-function coverage -------------------------

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid test date")
    }

    #[test]
    fn dates_to_roll_is_empty_when_last_rolled_is_already_yesterday() {
        let today = ymd(2026, 7, 6);
        let last_rolled = Some(ymd(2026, 7, 5)); // yesterday
        assert_eq!(dates_to_roll(last_rolled, today, 90), Vec::new());
    }

    #[test]
    fn dates_to_roll_backfills_every_day_in_a_multi_day_gap() {
        // Laptop asleep for 4 days: last successful rollup was 2026-07-01, today is
        // 2026-07-06 — every day from 07-02 through 07-05 (yesterday) must be
        // targeted, not just 07-05 alone.
        let today = ymd(2026, 7, 6);
        let last_rolled = Some(ymd(2026, 7, 1));
        let got = dates_to_roll(last_rolled, today, 90);
        assert_eq!(
            got,
            vec![ymd(2026, 7, 2), ymd(2026, 7, 3), ymd(2026, 7, 4), ymd(2026, 7, 5)],
            "every un-rolled date between last_rolled and yesterday must be included"
        );
    }

    #[test]
    fn dates_to_roll_from_fresh_install_starts_at_the_retention_floor() {
        // `None` (nothing ever rolled) must not walk back to the dawn of time —
        // bounded at `today - retention_days`, same floor as an old `last_rolled`.
        let today = ymd(2026, 7, 6);
        let got = dates_to_roll(None, today, 3);
        assert_eq!(got, vec![ymd(2026, 7, 3), ymd(2026, 7, 4), ymd(2026, 7, 5)]);
    }

    #[test]
    fn dates_to_roll_never_targets_a_date_older_than_the_retention_floor() {
        // last_rolled far outside the retention window (e.g. the worker was down for
        // a year) must clamp to the floor, not try to roll a date whose raw traces
        // `delete_traces_older_than` would already have pruned.
        let today = ymd(2026, 7, 6);
        let last_rolled = Some(ymd(2025, 1, 1));
        let got = dates_to_roll(last_rolled, today, 5);
        assert_eq!(got, vec![ymd(2026, 7, 1), ymd(2026, 7, 2), ymd(2026, 7, 3), ymd(2026, 7, 4), ymd(2026, 7, 5)]);
    }

    /// End-to-end (M7b): a multi-day gap in `kms_task_traces` — with NO rollup row
    /// for the middle days — must be fully backfilled by one `loop_forever` cycle,
    /// not just the single most-recent day. Proves the fix at the level success
    /// criteria actually care about: real rows land in `kms_daily_rollup` for every
    /// gap day, and accrual is "N distinct rolled days," not "N calendar days since
    /// install."
    #[tokio::test]
    async fn multi_day_gap_backfills_every_unrolled_date_not_just_yesterday() {
        use haily_db::queries::sessions;

        let (db, _dir) = test_db().await;
        let session_id = uuid::Uuid::new_v4().to_string();
        sessions::create_session(&db, &session_id, "test-adapter", None)
            .await
            .expect("create session");

        // Seed one trace each on 4 consecutive days ending 2 days ago — simulates a
        // laptop that was asleep and only just woke up, well before "yesterday."
        let today = Utc::now().date_naive();
        let seeded_dates: Vec<String> = (2..=5)
            .map(|days_ago| (today - Duration::days(days_ago)).format("%Y-%m-%d").to_string())
            .collect();
        for date in &seeded_dates {
            let trace = db_skills::insert_trace(
                &db,
                &session_id,
                "task",
                "[]",
                "success",
                Some(10),
                db_skills::TraceMetrics::default(),
            )
            .await
            .expect("insert trace");
            sqlx::query("UPDATE kms_task_traces SET created_at = ? WHERE id = ?")
                .bind(format!("{date}T09:00:00+00:00"))
                .bind(&trace.id)
                .execute(db.pool())
                .await
                .expect("backdate trace");
        }

        // No prior rollup exists — `latest_rollup_date` returns `None`, so the whole
        // gap (bounded by `RAW_RETENTION_DAYS`) must be walked in one cycle.
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), loop_forever(db.clone(), shutdown))
            .await
            .expect("one pre-cancelled cycle must complete promptly");

        for date in &seeded_dates {
            let rows = db_skills::rollup_for_date(&db, date)
                .await
                .expect("rollup_for_date");
            assert_eq!(
                rows.len(),
                1,
                "gap day {date} must have been backfilled, not skipped — got {rows:?}"
            );
            assert_eq!(rows[0].count, 1);
        }
    }
}
