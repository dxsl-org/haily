//! Worktree reaper — periodic background worker that garbage-collects abandoned coding
//! workspaces (Pipeline Activation & Wiring, phase 6).
//!
//! Two independent reconciliations run each tick:
//! 1. **Row reap** — `coding_workspaces::list_active` rows are torn down via
//!    `CodingWorkspace::discard` when their owning run is provably finished (terminal +
//!    past a grace window), or never got a run at all and has sat stale past a TTL with
//!    nothing live still pointing at it.
//! 2. **Filesystem reconcile** — directories under `worktrees_root` with no matching active
//!    row are crash orphans (the process died between `git worktree add` and the row
//!    commit, or between a row's soft-delete and its own `git worktree remove`) and are
//!    force-removed via git.
//!
//! Both steps are best-effort: a single failed reap/removal is logged and the loop moves on
//! to the next row/entry — never aborts the tick, never blocks shutdown. The critical safety
//! invariant is [`is_reapable`]: a workspace whose owning run is still non-terminal (or whose
//! session/work_item has ANY non-terminal run in flight) is never a candidate, regardless of
//! how old it is.
use chrono::{DateTime, Duration, Utc};
use haily_core::pipeline::RunStatus;
use haily_db::{
    queries::{
        coding_workspaces::{self, CodingWorkspaceRow},
        pipeline_runs,
    },
    DbHandle,
};
use haily_tools::coding::workspace::{object_dir_for, CodingWorkspace};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};

/// Tick cadence — hourly is coarse enough to be cheap yet bounds worktree accumulation to
/// within an hour of a run finishing (mirrors `spawn_journal_purge`'s own interval choice).
const TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Grace window after a workspace's owning run goes terminal (Done/Failed) before its
/// worktree is reaped. A `Done` run's workspace is normally discarded synchronously by
/// `launch_coding_run`'s finalize step the moment the run finishes, so this window mostly
/// matters for `Failed` runs (deliberately kept for user inspection) and the residual case
/// where that synchronous discard itself failed.
const TERMINAL_GRACE: Duration = Duration::hours(1);

/// TTL for a workspace whose owning run is `Paused` or has no `run_id` at all (opened but the
/// driving run crashed before reaching any status, or paused with no resume mechanism wired
/// yet — `finalize_workspace` never discards either case, on the theory a follow-up trigger
/// might resume them; today nothing implements that follow-up, so both classes are abandoned
/// after this TTL rather than held forever).
const NULL_RUN_TTL: Duration = Duration::hours(24);

/// Minimum on-disk age before a rowless directory under `worktrees_root` is treated as a crash
/// orphan. `CodingWorkspace::open` runs `git worktree add` (workspace.rs:63) before inserting the
/// owning DB row (workspace.rs:71) — a dir can legitimately exist with no active row for a brief
/// window while a launch is still in flight. Five minutes is generous against that gap (typically
/// milliseconds) and cheap against the hourly tick; a dir this young is skipped for THIS tick and
/// re-evaluated next tick, never force-removed on the strength of a single snapshot.
const FS_ORPHAN_GRACE: std::time::Duration = std::time::Duration::from_secs(300);

/// True if `path`'s mtime is at least `grace` old — old enough to treat an ownerless directory
/// as a genuine orphan rather than one still mid-`CodingWorkspace::open`. Unreadable metadata or
/// a clock-skewed future mtime fail closed (treated as "too young") — leaking a rare unremovable
/// orphan dir one more tick is cheaper than risking a live worktree. `grace` is a parameter (not
/// the `FS_ORPHAN_GRACE` constant directly) so tests can exercise both branches deterministically
/// instead of sleeping for real wall-clock minutes.
async fn old_enough_to_reap(path: &Path, grace: std::time::Duration) -> bool {
    match tokio::fs::metadata(path).await.and_then(|m| m.modified()) {
        Ok(modified) => modified.elapsed().map(|age| age >= grace).unwrap_or(false),
        Err(_) => false,
    }
}

/// Suffix marking a workspace's isolated git-object sibling dir (mirrors
/// `haily_tools::coding::workspace`'s private `OBJ_DIR_SUFFIX`). These are plain scratch
/// directories — never git-tracked, never a row's own id — so unlike a worktree dir proper
/// they are removed directly rather than via `git worktree remove`.
const OBJECT_DIR_SUFFIX: &str = ".git-objects";

/// Fixed, discoverable base dir for ephemeral per-run worktrees. MUST match
/// `haily_core::pipeline::launcher`'s own (private) `worktrees_root()` literal — duplicated
/// here rather than exported across the crate boundary because this phase's file ownership
/// does not include any `haily-core` file (see the phase's Deviation Log).
pub fn default_worktrees_root() -> PathBuf {
    std::env::temp_dir().join("haily-coding-worktrees")
}

/// Spawn the reaper loop. Registered on `tasks` and selecting on `shutdown`, mirroring every
/// other background worker in this crate (`spawn_work_item_watcher`, `spawn_journal_purge`).
pub fn spawn_worktree_reaper(
    db: Arc<DbHandle>,
    worktrees_root: PathBuf,
    shutdown: CancellationToken,
    tasks: TaskTracker,
) {
    tasks.spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("worktree reaper shutting down");
                    break;
                }
                _ = tokio::time::sleep(TICK_INTERVAL) => {}
            }
            reap_rows(&db).await;
            reconcile_filesystem(&db, &worktrees_root, FS_ORPHAN_GRACE).await;
        }
    });
}

/// Reap every active `coding_workspaces` row whose owning run has provably finished (or never
/// started one and has gone stale). One failed classification/discard is logged and the loop
/// continues onto the next row.
async fn reap_rows(db: &DbHandle) {
    let rows = match coding_workspaces::list_active(db).await {
        Ok(v) => v,
        Err(e) => {
            warn!("reaper: list_active workspaces failed: {e:#}");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }

    // Non-terminal runs, snapshotted once per tick — the live-run safety net for the NULL-
    // run-id branch (a workspace mid-flight has NULL `run_id` until its run finishes, so its
    // ONLY protection against a stale-looking reap is "does a non-terminal run for this
    // session/work_item still exist").
    let live_runs = match pipeline_runs::list_active(db).await {
        Ok(v) => v,
        Err(e) => {
            warn!("reaper: list_active pipeline_runs failed: {e:#}");
            return;
        }
    };
    let live_sessions: HashSet<String> = live_runs.iter().map(|r| r.session_id.clone()).collect();
    let live_work_items: HashSet<String> =
        live_runs.iter().filter_map(|r| r.work_item_id.clone()).collect();

    for row in &rows {
        let run_status = match &row.run_id {
            Some(run_id) => match pipeline_runs::status_of(db, run_id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(workspace = %row.id, "reaper: status_of failed: {e:#}");
                    continue;
                }
            },
            None => None,
        };

        if is_reapable(row, run_status.as_deref(), &live_sessions, &live_work_items, Utc::now()) {
            let ws = CodingWorkspace { row: row.clone() };
            match ws.discard(db).await {
                Ok(()) => info!(workspace = %row.id, "reaper: reaped abandoned workspace"),
                Err(e) => warn!(workspace = %row.id, "reaper: discard failed: {e:#}"),
            }
        }
    }
}

/// Pure classification (unit-testable without a DB or git). Fails CLOSED — never reaps — on
/// any ambiguity: an unparsable timestamp, an unknown/vanished run, or a non-terminal run.
fn is_reapable(
    row: &CodingWorkspaceRow,
    run_status: Option<&str>,
    live_sessions: &HashSet<String>,
    live_work_items: &HashSet<String>,
    now: DateTime<Utc>,
) -> bool {
    let Ok(updated_at) = DateTime::parse_from_rfc3339(&row.updated_at) else {
        return false;
    };
    let age = now - updated_at.with_timezone(&Utc);

    match &row.run_id {
        Some(_) => match run_status.and_then(RunStatus::parse) {
            // Done/Failed are `is_terminal()`; Interrupted (a user Stop) never resumes either
            // but is deliberately NOT `is_terminal()` at the `RunStatus` level (that flag means
            // "pipeline logic treats this as finished", which Interrupted isn't — a cancelled
            // run's stage state is just abandoned, not concluded). The reaper only cares that
            // nothing will ever touch this workspace again, which is true for both.
            Some(RunStatus::Done | RunStatus::Failed | RunStatus::Interrupted) => {
                age >= TERMINAL_GRACE
            }
            // Paused implies "might resume" but no resume trigger exists yet — bound it with
            // the same long TTL as a run_id-less row rather than holding it forever.
            Some(RunStatus::Paused) => age >= NULL_RUN_TTL,
            Some(RunStatus::Queued | RunStatus::Running) | None => false,
        },
        None => {
            let has_live_run = live_sessions.contains(&row.session_id)
                || row.work_item_id.as_ref().is_some_and(|w| live_work_items.contains(w));
            age >= NULL_RUN_TTL && !has_live_run
        }
    }
}

/// Reconcile `worktrees_root`'s on-disk entries against the live `coding_workspaces` set —
/// any directory with no matching active row is a crash orphan (or its now-parentless
/// isolated object-store sibling) and is removed.
async fn reconcile_filesystem(db: &DbHandle, worktrees_root: &Path, grace: std::time::Duration) {
    let active_ids: HashSet<String> = match coding_workspaces::list_active(db).await {
        Ok(rows) => rows.into_iter().map(|r| r.id).collect(),
        Err(e) => {
            warn!("reaper: list_active for filesystem reconcile failed: {e:#}");
            return;
        }
    };

    let mut entries = match tokio::fs::read_dir(worktrees_root).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            warn!("reaper: reading worktrees root failed: {e:#}");
            return;
        }
    };

    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                warn!("reaper: reading a worktrees-root entry failed: {e:#}");
                break;
            }
        };
        let Ok(file_type) = entry.file_type().await else { continue };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();

        if let Some(owner_id) = name.strip_suffix(OBJECT_DIR_SUFFIX) {
            // A workspace's own object-store sibling outlives its worktree only when
            // `discard`'s best-effort `remove_dir_all` failed — never git-tracked, so it is
            // always safe to remove directly, once old enough to rule out a fresh in-flight open.
            if !active_ids.contains(owner_id) && old_enough_to_reap(&entry.path(), grace).await {
                let _ = tokio::fs::remove_dir_all(entry.path()).await;
            }
            continue;
        }

        if active_ids.contains(&name) {
            continue;
        }
        if !old_enough_to_reap(&entry.path(), grace).await {
            // Too young to distinguish "mid-`CodingWorkspace::open`" from "genuine orphan" —
            // the DB row for a brand-new workspace lands a moment after its worktree dir does.
            continue;
        }
        remove_orphan_worktree(&entry.path(), &name).await;
    }
}

/// Force-remove one crash-orphaned worktree directory. The removal itself MUST run with `-C`
/// pointed OUTSIDE the directory being removed: `-C <path>` sets the spawned git process's own
/// cwd to `<path>`, and Windows refuses to delete a directory that is a live process's current
/// working directory — invoking `worktree remove` with `-C` at the target itself fails the
/// actual `rmdir` (while still unregistering the worktree from the repo's admin data), silently
/// leaving an unregistered-but-present directory behind (caught by a real-git integration test).
/// [`resolve_git_common_dir`] resolves the owning repo's path first via a read-only `rev-parse`
/// (safe to run from inside the worktree, since it never deletes anything), then the actual
/// removal runs `-C`'d at the REPO, not the worktree — mirroring `CodingWorkspace::discard`,
/// which always has that repo path from its DB row; the reaper has no such row here, so it must
/// derive the repo path from the worktree's own `.git` file instead.
async fn remove_orphan_worktree(path: &Path, id: &str) {
    let Some(path_str) = path.to_str() else {
        warn!(id, "reaper: orphan worktree path is not valid UTF-8, skipping");
        return;
    };

    let common_dir = match resolve_git_common_dir(path_str).await {
        Ok(d) => d,
        Err(e) => {
            warn!(id, "reaper: resolving orphan's owning repo failed: {e}");
            return;
        }
    };
    let Some(repo_dir) = common_dir.parent().and_then(Path::to_str) else {
        warn!(id, "reaper: orphan's resolved git-common-dir has no valid parent, skipping");
        return;
    };

    let out = tokio::process::Command::new("git")
        .args(["-C", repo_dir, "worktree", "remove", "--force", path_str])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => info!(id, "reaper: removed crash-orphan worktree"),
        Ok(o) => warn!(
            id,
            "reaper: git worktree remove failed for orphan: {}",
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(e) => warn!(id, "reaper: spawning git for orphan removal failed: {e:#}"),
    }
    // Never git-tracked — safe to remove directly regardless of the git step's outcome.
    let _ = tokio::fs::remove_dir_all(object_dir_for(path_str)).await;
}

/// Resolve the absolute path to the git repo that owns the worktree at `worktree_path` (the
/// parent of its shared `.git`/common dir) — read-only, so unlike the actual removal it is safe
/// to invoke with `-C` pointed AT the worktree itself.
async fn resolve_git_common_dir(worktree_path: &str) -> Result<PathBuf, String> {
    let out = tokio::process::Command::new("git")
        .args(["-C", worktree_path, "rev-parse", "--git-common-dir"])
        .output()
        .await
        .map_err(|e| format!("spawning git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let common_dir = PathBuf::from(&raw);
    Ok(if common_dir.is_absolute() {
        common_dir
    } else {
        Path::new(worktree_path).join(common_dir)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::queries::pipeline_runs::{self, RunTransition};

    // -----------------------------------------------------------------------------------
    // Pure classification (`is_reapable`) — no DB, no git.
    // -----------------------------------------------------------------------------------

    fn row(run_id: Option<&str>, work_item_id: Option<&str>, age: Duration) -> CodingWorkspaceRow {
        let ts = (Utc::now() - age).to_rfc3339();
        CodingWorkspaceRow {
            id: "ws1".into(),
            session_id: "sess1".into(),
            repo_path: "/repo".into(),
            branch: "haily/ws-1".into(),
            worktree_path: "/wt/ws1".into(),
            work_item_id: work_item_id.map(str::to_string),
            created_at: ts.clone(),
            updated_at: ts,
            deleted_at: None,
            run_id: run_id.map(str::to_string),
        }
    }

    #[test]
    fn terminal_run_past_grace_is_reapable() {
        let r = row(Some("run1"), None, TERMINAL_GRACE + Duration::minutes(5));
        assert!(is_reapable(&r, Some("failed"), &HashSet::new(), &HashSet::new(), Utc::now()));
    }

    #[test]
    fn terminal_run_within_grace_is_not_yet_reapable() {
        let r = row(Some("run1"), None, Duration::minutes(1));
        assert!(!is_reapable(&r, Some("done"), &HashSet::new(), &HashSet::new(), Utc::now()));
    }

    #[test]
    fn in_flight_run_is_never_reapable_regardless_of_age() {
        // The critical safety invariant: an actually-in-flight run's workspace must never be
        // torn down, even if it looks very old.
        let r = row(Some("run1"), None, Duration::days(30));
        for status in ["queued", "running"] {
            assert!(
                !is_reapable(&r, Some(status), &HashSet::new(), &HashSet::new(), Utc::now()),
                "status {status} must never be reaped"
            );
        }
    }

    #[test]
    fn interrupted_run_is_reaped_like_failed_once_past_grace() {
        // A user Stop never resumes, same as Failed — just via a different code path
        // (RunStatus::is_terminal() intentionally excludes Interrupted at the pipeline-status
        // level, but the reaper's concern is "will anything ever touch this again", not that flag).
        let fresh = row(Some("run1"), None, Duration::minutes(1));
        assert!(!is_reapable(&fresh, Some("interrupted"), &HashSet::new(), &HashSet::new(), Utc::now()));

        let aged = row(Some("run1"), None, TERMINAL_GRACE + Duration::minutes(5));
        assert!(is_reapable(&aged, Some("interrupted"), &HashSet::new(), &HashSet::new(), Utc::now()));
    }

    #[test]
    fn paused_run_is_reaped_after_the_long_ttl_not_the_short_grace() {
        // Paused nominally means "might resume" — no resume trigger exists yet, so it must not
        // be held forever, but it gets the same long runway as a run_id-less row rather than
        // the short terminal grace (a paused run is likelier to still be actionable soon).
        let within_terminal_grace_but_not_ttl = row(Some("run1"), None, TERMINAL_GRACE + Duration::minutes(5));
        assert!(!is_reapable(
            &within_terminal_grace_but_not_ttl,
            Some("paused"),
            &HashSet::new(),
            &HashSet::new(),
            Utc::now()
        ));

        let past_ttl = row(Some("run1"), None, NULL_RUN_TTL + Duration::minutes(5));
        assert!(is_reapable(&past_ttl, Some("paused"), &HashSet::new(), &HashSet::new(), Utc::now()));
    }

    #[test]
    fn unknown_or_vanished_run_status_is_never_reapable() {
        let r = row(Some("run1"), None, Duration::days(30));
        assert!(!is_reapable(&r, None, &HashSet::new(), &HashSet::new(), Utc::now()));
    }

    #[test]
    fn null_run_aged_past_ttl_with_no_live_run_is_reapable() {
        let r = row(None, None, NULL_RUN_TTL + Duration::minutes(5));
        assert!(is_reapable(&r, None, &HashSet::new(), &HashSet::new(), Utc::now()));
    }

    #[test]
    fn null_run_aged_but_live_session_run_is_not_reapable() {
        let r = row(None, None, NULL_RUN_TTL + Duration::minutes(5));
        let mut live = HashSet::new();
        live.insert("sess1".to_string());
        assert!(!is_reapable(&r, None, &live, &HashSet::new(), Utc::now()));
    }

    #[test]
    fn null_run_aged_but_live_work_item_run_is_not_reapable() {
        let r = row(None, Some("wi1"), NULL_RUN_TTL + Duration::minutes(5));
        let mut live_wi = HashSet::new();
        live_wi.insert("wi1".to_string());
        assert!(!is_reapable(&r, None, &HashSet::new(), &live_wi, Utc::now()));
    }

    #[test]
    fn null_run_not_yet_past_ttl_is_not_reapable() {
        let r = row(None, None, Duration::hours(1));
        assert!(!is_reapable(&r, None, &HashSet::new(), &HashSet::new(), Utc::now()));
    }

    #[test]
    fn unparsable_timestamp_fails_closed() {
        let mut r = row(Some("run1"), None, Duration::days(30));
        r.updated_at = "not-a-timestamp".into();
        assert!(!is_reapable(&r, Some("done"), &HashSet::new(), &HashSet::new(), Utc::now()));
    }

    // -----------------------------------------------------------------------------------
    // End-to-end: real DB + real git worktrees.
    // -----------------------------------------------------------------------------------

    async fn git(dir: &Path, args: &[&str]) {
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .await
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    async fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-b", "main"]).await;
        git(dir.path(), &["config", "user.email", "t@haily.test"]).await;
        git(dir.path(), &["config", "user.name", "Test"]).await;
        tokio::fs::write(dir.path().join("README.md"), "hello\n").await.unwrap();
        git(dir.path(), &["add", "."]).await;
        git(dir.path(), &["commit", "-m", "init"]).await;
        dir
    }

    async fn test_db() -> (tempfile::TempDir, Arc<DbHandle>, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());
        let session_id = uuid::Uuid::new_v4().to_string();
        haily_db::queries::sessions::create_session(&db, &session_id, "coding", None)
            .await
            .unwrap();
        (dir, db, session_id)
    }

    /// Backdate a workspace row's `updated_at` directly — mirrors the codebase's own idiom for
    /// aging a row in tests (e.g. `haily-db/tests/skills.rs`).
    async fn backdate_workspace(db: &DbHandle, id: &str, age: Duration) {
        let ts = (Utc::now() - age).to_rfc3339();
        sqlx::query("UPDATE coding_workspaces SET updated_at = ? WHERE id = ?")
            .bind(&ts)
            .bind(id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    async fn make_run(db: &DbHandle, session_id: &str, status: &str) -> String {
        let run = pipeline_runs::create(db, session_id, None, 5).await.unwrap();
        pipeline_runs::transition(
            db,
            &run.id,
            RunTransition {
                stage_index: 0,
                status,
                attempt: 0,
                attempts_remaining: 5,
                tier_used: None,
                backend_used: None,
                egress: None,
                gate_output_digest: None,
            },
        )
        .await
        .unwrap();
        run.id
    }

    /// Critical safety test: a terminal (`failed`), aged-past-grace workspace is reaped for
    /// real (worktree gone, row soft-deleted), while a workspace whose `run_id` references a
    /// still-`running` run — equally old — is left completely untouched.
    #[tokio::test]
    async fn reap_rows_discards_terminal_but_never_touches_an_in_flight_run() {
        let repo = init_repo().await;
        let (_dbdir, db, session_id) = test_db().await;
        let wt_root = tempfile::tempdir().unwrap();

        let ws_failed =
            CodingWorkspace::open(&db, &session_id, repo.path(), wt_root.path(), None)
                .await
                .unwrap();
        let run_failed = make_run(&db, &session_id, "failed").await;
        coding_workspaces::set_run_id(&db, &ws_failed.row.id, &run_failed).await.unwrap();
        backdate_workspace(&db, &ws_failed.row.id, TERMINAL_GRACE + Duration::minutes(5)).await;

        let ws_running =
            CodingWorkspace::open(&db, &session_id, repo.path(), wt_root.path(), None)
                .await
                .unwrap();
        let run_running = make_run(&db, &session_id, "running").await;
        coding_workspaces::set_run_id(&db, &ws_running.row.id, &run_running).await.unwrap();
        backdate_workspace(&db, &ws_running.row.id, TERMINAL_GRACE + Duration::minutes(5)).await;

        reap_rows(&db).await;

        assert!(
            coding_workspaces::get(&db, &ws_failed.row.id).await.unwrap().is_none(),
            "terminal+aged workspace row must be soft-deleted"
        );
        assert!(
            !ws_failed.worktree_root().exists(),
            "terminal+aged workspace worktree must be removed from disk"
        );

        assert!(
            coding_workspaces::get(&db, &ws_running.row.id).await.unwrap().is_some(),
            "an in-flight run's workspace row must never be reaped"
        );
        assert!(
            ws_running.worktree_root().is_dir(),
            "an in-flight run's worktree must never be touched"
        );
    }

    /// A crash-orphan directory (no matching active row) is force-removed via
    /// `git worktree remove`, while a live worktree with an active row is left untouched.
    #[tokio::test]
    async fn reconcile_filesystem_removes_orphan_and_preserves_active_worktree() {
        let repo = init_repo().await;
        let (_dbdir, db, session_id) = test_db().await;
        let wt_root = tempfile::tempdir().unwrap();

        let ws_live = CodingWorkspace::open(&db, &session_id, repo.path(), wt_root.path(), None)
            .await
            .unwrap();

        // Simulate a crash orphan: a real worktree with no corresponding DB row (as if the
        // process died between `git worktree add` and the row insert).
        let orphan_id = uuid::Uuid::new_v4().to_string();
        let orphan_path = wt_root.path().join(&orphan_id);
        let orphan_path_str = orphan_path.to_str().unwrap();
        git(
            repo.path(),
            &["worktree", "add", "-b", &format!("haily/orphan-{orphan_id}"), orphan_path_str, "HEAD"],
        )
        .await;
        assert!(orphan_path.is_dir(), "fixture setup: orphan worktree must exist before reconcile");

        // grace=ZERO: this test asserts the STEADY-STATE orphan case (dir has existed for a
        // while with no row) — the grace window itself is covered separately below.
        reconcile_filesystem(&db, wt_root.path(), std::time::Duration::ZERO).await;

        assert!(!orphan_path.exists(), "crash-orphan worktree must be removed");
        assert!(ws_live.worktree_root().is_dir(), "a live worktree with an active row must survive");
        assert!(
            coding_workspaces::get(&db, &ws_live.row.id).await.unwrap().is_some(),
            "the live workspace's row must be untouched"
        );
    }

    /// A rowless directory younger than the grace window is preserved, not reaped — this is the
    /// window between `CodingWorkspace::open`'s `git worktree add` and its DB row insert
    /// (workspace.rs:63,71), where a brand-new in-flight launch briefly looks identical to a
    /// crash orphan. Reaping it here would delete a live run's worktree out from under it.
    #[tokio::test]
    async fn reconcile_filesystem_preserves_a_rowless_dir_younger_than_the_grace_window() {
        let repo = init_repo().await;
        let (_dbdir, db, _session_id) = test_db().await;
        let wt_root = tempfile::tempdir().unwrap();

        let fresh_id = uuid::Uuid::new_v4().to_string();
        let fresh_path = wt_root.path().join(&fresh_id);
        let fresh_path_str = fresh_path.to_str().unwrap();
        git(
            repo.path(),
            &["worktree", "add", "-b", &format!("haily/fresh-{fresh_id}"), fresh_path_str, "HEAD"],
        )
        .await;
        assert!(fresh_path.is_dir(), "fixture setup: fresh worktree must exist before reconcile");

        // Default grace (FS_ORPHAN_GRACE) — the dir is milliseconds old, nowhere near 5 minutes.
        reconcile_filesystem(&db, wt_root.path(), FS_ORPHAN_GRACE).await;

        assert!(
            fresh_path.is_dir(),
            "a rowless dir younger than the grace window must survive this tick"
        );
    }
}
