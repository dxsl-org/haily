//! `kill_run`/`pause_run`/`resume_run` (Unified Chat UI phase 6, D3) — local-GUI-only Tauri
//! command bodies (never bridged to the mobile/remote surface: a bare `run_id` carries no
//! session/ownership binding). Kill/pause never touch `safety.disable_writes`; resume re-enters
//! normal approval gating (no elevated permission).
use crate::bootstrap::AppHandle;
use crate::run_control::{LaunchCtx, RunControlRegistry};
use anyhow::{bail, Result};
use haily_core::{CodingRunSpec, RunKind};
use haily_db::queries::{coding_workspaces, pipeline_runs};
use haily_db::DbHandle;
use haily_types::{DepthMode, ResponseChunk};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

/// Fresh attempts budget a resumed run relaunches with (mirrors
/// `pipeline::launcher::DEFAULT_ATTEMPTS_BUDGET` — duplicated rather than exported, since that
/// constant is private to a `haily-core` module this crate must not reach into).
const RESUME_ATTEMPTS_BUDGET: i64 = 8;

/// Cancel `run_id`: fires its registered token (immediate — a stage sub-turn is a child of it,
/// so an in-flight stage stops right away, not just at the next boundary) AND, if the row is
/// still non-terminal, soft-deletes it as a checkpoint fallback for a run whose token is not
/// registered (e.g. resumed after a restart with no live registry entry). The runner's own
/// between-stage/`transition` row-alive check already treats a vanished row as `Interrupted`
/// (see `PipelineRunner::run`'s `stage_row_alive` handling) — the soft-delete is what makes that
/// check fire. Never touches `safety.disable_writes`.
///
/// Returns `true` if EITHER the token fired or the row was soft-deleted; `false` if `run_id` was
/// already terminal/unknown (nothing to do).
///
/// # Errors
/// Returns an error if the DB query fails.
pub async fn kill_run(db: &DbHandle, registry: &RunControlRegistry, run_id: &str) -> Result<bool> {
    let token_fired = registry.cancel(run_id);
    let row_deleted = match pipeline_runs::get(db, run_id).await? {
        Some(row)
            if matches!(
                row.status.as_str(),
                "queued" | "running" | "paused" | "interrupted"
            ) =>
        {
            pipeline_runs::soft_delete(db, run_id).await?
        }
        _ => false,
    };
    Ok(token_fired || row_deleted)
}

/// Set `run_id`'s pause flag. Best-effort, stage-boundary only (never mid-stage) — the runner
/// observes it at the same checkpoint as `kill`/`cancel` and transitions the row to `paused`
/// with an `explicit_stop` reason class itself; this function does no DB write of its own.
///
/// Returns `false` if `run_id` has no registered (live) entry — already terminal/paused, or
/// unknown.
pub fn pause_run(registry: &RunControlRegistry, run_id: &str) -> bool {
    registry.set_pause(run_id)
}

/// Resume `run_id`: eligible iff `status = interrupted`, or `status = paused` with a
/// `retries_exhausted`/`explicit_stop` reason class — never an approval-wait pause (resolves
/// through its approval card) nor a terminal/live row. On pass: verifies the owning workspace
/// row AND its on-disk worktree still exist (refuses with a clear error otherwise — this is ALSO
/// the no-double-apply guard: a successful `worktree_apply` removes the worktree directory as
/// its last step, so a vanished worktree with a live workspace row means the ship already fully
/// applied and must never be re-emitted), atomically resets the row to `running` at a fresh
/// attempts budget, reconstructs the originating `CodingRunSpec` from the row's persisted
/// `task`/`run_kind`/`depth`, and relaunches via [`super::spawn_launch`] — so the resumed run
/// re-registers a token/pause pair under the SAME `run_id`. Binds the row's (reused, not
/// freshly-minted) `session_id` to the `"gui"` adapter and spawns its own chunk-forwarding loop
/// (mirrors `cockpit::start_coding_run`'s own wiring) — the caller need not know `session_id` in
/// advance, since a resume reuses whichever session the original launch was bound to, not a new
/// one the frontend picked.
///
/// Returns `Ok(false)` (never an error) for every ordinary "nothing to do" case: unknown id,
/// wrong status/reason class, or no resume context on the row (a pre-migration/eval row).
/// Returns `Err` only for the reaped/already-applied workspace case (a clear message the caller
/// should surface verbatim) or a genuine DB/query failure.
///
/// # Errors
/// Returns an error if the workspace was already reclaimed (worktree gone) or a DB query fails.
pub async fn resume_run(app: &AppHandle, run_id: &str) -> Result<bool> {
    let Some(row) = pipeline_runs::get(&app.db, run_id).await? else {
        return Ok(false);
    };
    let resumable = row.status == "interrupted"
        || (row.status == "paused"
            && matches!(
                row.pause_reason_class.as_deref(),
                Some("retries_exhausted") | Some("explicit_stop")
            ));
    if !resumable {
        return Ok(false);
    }
    let (Some(task), Some(run_kind), Some(depth)) = (
        row.task.as_deref(),
        row.run_kind.as_deref(),
        row.depth.as_deref(),
    ) else {
        // A row from before this migration, or from a caller with no resume context (eval/test)
        // — nothing to reconstruct a relaunch from.
        return Ok(false);
    };

    let Some(workspace) = coding_workspaces::find_by_session(&app.db, &row.session_id).await?
    else {
        return Ok(false);
    };
    let worktree_exists = tokio::fs::metadata(&workspace.worktree_path)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);
    if !worktree_exists {
        bail!(
            "Không thể tiếp tục: workspace của run này đã bị dọn dẹp hoặc thay đổi đã được áp \
             dụng — không thể áp dụng lại một cách an toàn."
        );
    }

    let Some(reset) = pipeline_runs::resume_reset(&app.db, run_id, RESUME_ATTEMPTS_BUDGET).await?
    else {
        // Lost a race (e.g. the reaper or a concurrent resume_run already changed the row).
        return Ok(false);
    };

    let Ok(run_uuid) = Uuid::parse_str(run_id) else {
        return Ok(false);
    };
    let Ok(session_id) = Uuid::parse_str(&reset.session_id) else {
        return Ok(false);
    };
    let spec = CodingRunSpec {
        run_id: run_uuid,
        kind: RunKind::from_str_label(run_kind),
        task: task.to_string(),
        session_id,
        work_item_id: reset.work_item_id.clone(),
        repo_path: Some(PathBuf::from(&workspace.repo_path)),
        depth: DepthMode::from_label(depth),
    };

    app.adapters.bind_session(session_id, "gui");

    // Mirrors `cockpit::start_coding_run`'s own resp_rx-forwarding-to-adapters loop: the
    // frontend never sees this channel, only the `haily-chunk`/`haily-run-events` events it
    // ultimately produces via the SAME delivery path any other turn uses.
    let (resp_tx, mut resp_rx) = tokio::sync::mpsc::channel::<ResponseChunk>(256);
    let adapters = app.adapters.clone();
    // `tasks` is `pub(crate)` on `AppHandle` — accessible here since this module lives in the
    // SAME crate (mirrors `launch.rs`'s own direct field access).
    app.tasks.clone().spawn(async move {
        while let Some(chunk) = resp_rx.recv().await {
            let done = matches!(chunk, ResponseChunk::Complete);
            adapters.deliver(session_id, chunk).await.ok();
            if done {
                break;
            }
        }
        adapters.unbind_session(&session_id);
    });

    // `shutdown` is `pub(crate)` on `AppHandle` — see the note above.
    let ctx = LaunchCtx {
        orc: Arc::clone(&app.orchestrator),
        am: app.adapters.clone(),
        tasks: app.tasks.clone(),
        db: Arc::clone(&app.db),
        registry: Arc::clone(&app.run_control),
    };
    super::spawn_launch(ctx, spec, app.shutdown.child_token(), resp_tx);
    Ok(true)
}

#[cfg(test)]
mod tests;
