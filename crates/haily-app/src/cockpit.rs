//! GUI cockpit read/action surface (Sub-Agent + Skill Architecture phase 11a).
//!
//! The app-layer backing for the Tauri commands P11b's Svelte components call: the skills
//! browser, the workspace/sandbox panel, and the diff-view read side. Pure delegation —
//! `src-tauri` stays glue-only, every DB/git/kms call and every DTO shape lives here.
//!
//! ## Skill enable/pin persistence (LOCKED — see phase Deviation Log [P11a])
//! Enable/pin state is stored in the `meta` preferences table keyed by skill NAME
//! (`skill.enabled.<name>` / `skill.pinned.<name>`), so ONE mechanism covers both the
//! in-memory authored kit-pack AND the DB-synthesized skills without a schema migration.
//! `list_skills` reflects it and the setters write it. ENFORCEMENT — excluding a disabled
//! skill from injection, prioritizing a pinned one — was deferred here in P11a and is now
//! LIVE (Pipeline Activation phase 5): `haily-kms::skill_gates::load` reads this same `meta`
//! state and `haily-core::agent::sub_turn::run_sub_turn` threads it into both hot injection
//! paths (sync in-memory authored + async DB synthesized). The key-prefix constants below
//! are re-exported FROM `haily-kms` (not redeclared) so the setters here and that reader can
//! never silently drift apart.

use anyhow::{bail, Result};
use haily_core::{CodingRunSpec, RunKind};
use haily_db::queries::{coding_workspaces, meta, pipeline_runs, skills as db_skills};
use haily_db::DbHandle;
use haily_kms::skill_gates::{SKILL_ENABLED_PREFIX, SKILL_PINNED_PREFIX};
use haily_kms::KmsHandle;
use haily_tools::coding::workspace::CodingWorkspace;
use haily_tools::exec::{Manager, SandboxKind};
use haily_types::{DepthMode, ResponseChunk};
use serde::Serialize;
use std::path::PathBuf;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::bootstrap::AppHandle;

/// Cap on a workspace diff returned to the GUI, so a huge generated diff cannot flood the
/// IPC channel (the frontend virtualizes/paginates — phase risk note). 512 KiB is ample
/// for review; beyond it the diff is truncated with a trailing notice.
const MAX_DIFF_BYTES: usize = 512 * 1024;

/// One skill row for the cockpit skills browser. `source` distinguishes the trusted,
/// sha256-pinned authored kit-pack from the EMA/decay-lifecycle synthesized skills — the
/// confidence/use fields are populated only for the latter (authored skills have no such
/// lifecycle). `enabled`/`pinned` are the persisted admin state (see module doc).
#[derive(Debug, Clone, Serialize)]
pub struct SkillView {
    pub name: String,
    /// `"authored"` or `"synthesized"`.
    pub source: String,
    pub description: String,
    /// Authored-skill kind (`stage-prompt`/`playbook`/`standard`); `None` for synthesized.
    pub kind: Option<String>,
    /// EMA confidence — synthesized only.
    pub confidence: Option<f64>,
    /// Lifetime activation count — synthesized only.
    pub use_count: Option<i64>,
    /// RFC3339 last activation — synthesized only.
    pub last_used_at: Option<String>,
    pub enabled: bool,
    pub pinned: bool,
}

/// One active coding workspace for the Workspaces screen (phase 11a; extended Unified Chat UI
/// phase 10 with the linked run's status and the plain-language resume gate). Git-specific
/// fields (`branch`, `worktree_path`) are carried on the wire for the Settings → Advanced
/// disclosure ONLY — the default Workspaces screen must never render them (D6).
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceView {
    pub id: String,
    pub session_id: String,
    pub repo_path: String,
    pub branch: String,
    pub worktree_path: String,
    pub work_item_id: Option<String>,
    pub created_at: String,
    /// Whether the worktree has uncommitted changes. `false` when [`Self::worktree_reclaimed`]
    /// is `true` (nothing to be "dirty" about) or on a status-probe failure (logged) rather
    /// than blocking the whole list.
    pub dirty: bool,
    /// Count of changed paths (`git status --porcelain` lines) — `0` when reclaimed or clean.
    pub changed_file_count: i64,
    /// The host's active sandbox backend name (`"wsl2"`/`"null"`/…). Host-global, repeated
    /// per row so a panel can badge each workspace.
    pub sandbox_kind: String,
    /// `false` for the non-enforcing `NullSandbox` — the signal the panel must surface as a
    /// "first-exec approval required, not isolated" warning.
    pub sandbox_enforcing: bool,
    /// The pipeline run driving this workspace, if any is known yet (Unified Chat UI phase 10).
    /// `None` until either `coding_workspaces.run_id` is stamped (after the run goes
    /// terminal/paused) or an in-flight run is found for this workspace's session.
    pub run_id: Option<String>,
    /// The originating task text (`pipeline_runs.task`), for the row's "Bản làm việc riêng cho
    /// {task}" label. `None` for a row with no resume context (pre-migration/eval row) or no
    /// linked run at all — the frontend falls back to a generic label.
    pub task: Option<String>,
    /// Raw `pipeline_runs.status` of the linked run (`queued`/`running`/`paused`/`interrupted`/
    /// `done`/`failed`), `None` if no run is linked. The plain-language status mapping
    /// (`WorkspaceStatus.ts`) is a pure frontend function of this plus `dirty`/`worktree_reclaimed`.
    pub run_status: Option<String>,
    /// `pipeline_runs.pause_reason_class`, `None` unless `run_status == "paused"`.
    pub pause_reason_class: Option<String>,
    /// `true` when the worktree directory itself is gone (a completed `worktree_apply` removes
    /// it as its last step, or the reaper reclaimed it) while the `coding_workspaces` row is
    /// still active — the "đã dọn dẹp" state. Distinct from a merely-clean tree.
    pub worktree_reclaimed: bool,
    /// Whether "Tiếp tục" (Continue → `resume_run`) should be offered for this row: the linked
    /// run passes [`crate::run_control::is_resumable`] AND the worktree has not been reclaimed.
    /// Computed HERE (not re-derived in the frontend) so the enable rule can never drift from
    /// what `resume_run` itself would actually accept.
    pub resumable: bool,
}

/// Stable lowercase name for a sandbox backend, for the wire.
fn sandbox_kind_name(kind: SandboxKind) -> &'static str {
    match kind {
        SandboxKind::Wsl2 => "wsl2",
        SandboxKind::MacSeatbelt => "mac-seatbelt",
        SandboxKind::LinuxNamespace => "linux-namespace",
        SandboxKind::Wasm => "wasm",
        SandboxKind::Null => "null",
    }
}

/// Merge authored + synthesized skills into the browser view, applying the persisted
/// enable/pin state read once from the `meta` table (phase 11a). Name-sorted, authored
/// first within a name tie. A DB read failure for the synthesized set or the prefs yields
/// an empty/default contribution rather than an error — the browser must still render.
pub async fn list_skills(db: &DbHandle, kms: &KmsHandle) -> Result<Vec<SkillView>> {
    let enabled_prefs = meta::list_by_prefix(db, SKILL_ENABLED_PREFIX)
        .await
        .unwrap_or_default();
    let pinned_prefs = meta::list_by_prefix(db, SKILL_PINNED_PREFIX)
        .await
        .unwrap_or_default();

    // A skill is enabled UNLESS an explicit `false` pref exists (default-on); pinned only
    // when an explicit `true` pref exists (default-off).
    let is_enabled = |name: &str| {
        !enabled_prefs
            .iter()
            .any(|p| p.key == format!("{SKILL_ENABLED_PREFIX}{name}") && p.value == "false")
    };
    let is_pinned = |name: &str| {
        pinned_prefs
            .iter()
            .any(|p| p.key == format!("{SKILL_PINNED_PREFIX}{name}") && p.value == "true")
    };

    let mut out: Vec<SkillView> = Vec::new();

    for a in kms.authored_skills_list() {
        out.push(SkillView {
            enabled: is_enabled(&a.name),
            pinned: is_pinned(&a.name),
            source: "authored".to_string(),
            kind: Some(a.kind),
            confidence: None,
            use_count: None,
            last_used_at: None,
            description: a.description,
            name: a.name,
        });
    }

    for s in db_skills::active_skills(db).await.unwrap_or_default() {
        out.push(SkillView {
            enabled: is_enabled(&s.name),
            pinned: is_pinned(&s.name),
            source: "synthesized".to_string(),
            kind: None,
            confidence: Some(s.confidence),
            use_count: Some(s.use_count),
            last_used_at: s.last_used_at,
            description: s.description,
            name: s.name,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.source.cmp(&b.source)));
    Ok(out)
}

/// Persist a skill's enabled state (phase 11a). Writing the default (`enabled = true`)
/// deletes the pref row so absence == default; disabling writes `"false"`. See the module
/// doc for the deferred-enforcement contract.
///
/// # Errors
/// Returns an error if the preference write/delete fails.
pub async fn set_skill_enabled(db: &DbHandle, name: &str, enabled: bool) -> Result<()> {
    let key = format!("{SKILL_ENABLED_PREFIX}{name}");
    if enabled {
        meta::delete_preference(db, &key).await
    } else {
        meta::upsert_preference(db, &key, "false", "gui").await
    }
}

/// Persist a skill's pinned state (phase 11a). Pinning writes `"true"`; unpinning deletes
/// the pref row (absence == not pinned).
///
/// # Errors
/// Returns an error if the preference write/delete fails.
pub async fn pin_skill(db: &DbHandle, name: &str, pinned: bool) -> Result<()> {
    let key = format!("{SKILL_PINNED_PREFIX}{name}");
    if pinned {
        meta::upsert_preference(db, &key, "true", "gui").await
    } else {
        meta::delete_preference(db, &key).await
    }
}

/// List active coding workspaces with per-workspace change status, the host sandbox posture,
/// and the linked pipeline run's status/resume eligibility (phase 11a; extended Unified Chat UI
/// phase 10). The sandbox kind is probed ONCE (host-global) and stamped on each row; the change
/// summary is probed per workspace and fails soft to a clean/non-reclaimed default on a git
/// error (logged), so one broken worktree never blanks the whole screen.
///
/// # Errors
/// Returns an error only if the initial `coding_workspaces::list_active` query fails.
pub async fn list_workspaces(db: &DbHandle) -> Result<Vec<WorkspaceView>> {
    let kind = Manager::probe_sandbox_kind();
    let sandbox_kind = sandbox_kind_name(kind).to_string();
    let sandbox_enforcing = kind != SandboxKind::Null;

    let rows = coding_workspaces::list_active(db).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let ws = CodingWorkspace { row };

        let change = match ws.change_summary().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(workspace = %ws.row.id, "change summary probe failed, defaulting to clean: {e:#}");
                Some(haily_tools::coding::workspace::WorkspaceChangeSummary {
                    changed_file_count: 0,
                    dirty: false,
                })
            }
        };
        let worktree_reclaimed = change.is_none();
        let (changed_file_count, dirty) = change
            .map(|c| (c.changed_file_count as i64, c.dirty))
            .unwrap_or((0, false));

        // The workspace's own stamped `run_id` once its driving run is terminal/paused; while
        // still in flight (no `run_id` yet — see `set_run_id`'s doc), fall back to the
        // session's currently-active run.
        let run = match &ws.row.run_id {
            Some(run_id) => pipeline_runs::get(db, run_id).await?,
            None => pipeline_runs::find_active_by_session(db, &ws.row.session_id).await?,
        };
        let resumable = run.as_ref().is_some_and(|r| {
            !worktree_reclaimed
                && crate::run_control::is_resumable(&r.status, r.pause_reason_class.as_deref())
        });

        out.push(WorkspaceView {
            id: ws.row.id.clone(),
            session_id: ws.row.session_id.clone(),
            repo_path: ws.row.repo_path.clone(),
            branch: ws.row.branch.clone(),
            worktree_path: ws.row.worktree_path.clone(),
            work_item_id: ws.row.work_item_id.clone(),
            created_at: ws.row.created_at.clone(),
            dirty,
            changed_file_count,
            sandbox_kind: sandbox_kind.clone(),
            sandbox_enforcing,
            run_id: run.as_ref().map(|r| r.id.clone()),
            task: run.as_ref().and_then(|r| r.task.clone()),
            run_status: run.as_ref().map(|r| r.status.clone()),
            pause_reason_class: run.as_ref().and_then(|r| r.pause_reason_class.clone()),
            worktree_reclaimed,
            resumable,
        });
    }
    Ok(out)
}

/// Discard a workspace (revert worktree, remove it, delete the branch, soft-delete the
/// row) — the panel's Discard action (phase 11a). SESSION-SCOPED: a workspace id belonging
/// to a different session resolves to `None` and returns `false`, never touching it.
/// Returns `true` if a matching active workspace was discarded, `false` if none matched.
///
/// # Errors
/// Returns an error if the scoped lookup fails or the discard's row soft-delete fails.
pub async fn discard_workspace(db: &DbHandle, id: &str, session_id: &str) -> Result<bool> {
    let Some(row) = coding_workspaces::get_scoped(db, id, session_id).await? else {
        return Ok(false);
    };
    CodingWorkspace { row }.discard(db).await?;
    Ok(true)
}

/// The worktree's current unified diff against HEAD for the DiffViewer's read side (phase
/// 11a). SESSION-SCOPED like `discard_workspace`. The ACCEPT side is NOT here — accepting a
/// run's changes routes through the existing `worktree_apply` tool approval (view + accept
/// only, no editor). The returned text is untrusted repo content, capped at
/// [`MAX_DIFF_BYTES`]; the caller renders it as inert data.
///
/// # Errors
/// Returns an error if the scoped lookup or the git diff fails. A missing/foreign id
/// returns `Ok(None)`.
pub async fn workspace_diff(db: &DbHandle, id: &str, session_id: &str) -> Result<Option<String>> {
    let Some(row) = coding_workspaces::get_scoped(db, id, session_id).await? else {
        return Ok(None);
    };
    let diff = CodingWorkspace { row }.unified_diff(MAX_DIFF_BYTES).await?;
    Ok(Some(diff))
}

/// Launch a coding-pipeline run from the GUI's "New run" form (Pipeline Activation & Wiring
/// phase 3) — the GUI's own trigger onto the P1 launch entrypoint (`crate::launch_coding_run`).
/// Mints a fresh `session_id` (mirrors `send_message`: every GUI-initiated unit of work gets
/// its own id) and binds it to the `"gui"` adapter BEFORE launching — `launch_coding_run`'s
/// internal `spawn_run_event_bridge`/`spawn_distillation_bridge` deliver through this exact
/// binding, and `AdapterManager::deliver_run_event` errors on an unbound session. The delivery
/// loop below mirrors `dispatch::dispatch_loop`'s own per-turn forwarder (chunk-drain until
/// `Complete`, then unbind) so a launched run behaves identically to a normal chat turn from
/// the adapter's point of view — no new event channel, no bypass of the terminal-chunk
/// contract `AppHandle::shutdown`'s drain relies on.
///
/// Returns the minted session id so the caller can log/correlate it; `RunTimeline` itself is
/// session-agnostic (it renders every observed `run_id` regardless of session), so the
/// frontend does not need to thread this id anywhere further.
///
/// # Errors
/// Returns an error only for an unrecognized `kind` string. The launch itself never fails
/// synchronously — `crate::launch_coding_run` fires the run on a tracked background task and
/// reports a setup failure as a `ResponseChunk::Error` on the bound session instead (see its
/// own doc comment).
pub fn start_coding_run(
    app: &AppHandle,
    kind: &str,
    task: String,
    repo_path: Option<PathBuf>,
    depth: DepthMode,
) -> Result<Uuid> {
    let kind = match kind {
        "plan" => RunKind::Plan,
        "build" => RunKind::Build,
        other => bail!("unknown coding-run kind '{other}' (expected 'plan' or 'build')"),
    };

    let session_id = Uuid::new_v4();
    app.adapters.bind_session(session_id, "gui");

    let (resp_tx, mut resp_rx) = mpsc::channel::<ResponseChunk>(256);
    let adapters = app.adapters.clone();
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

    let spec = CodingRunSpec {
        // Overwritten by `launch_coding_run` with a fresh id before registration (Unified Chat
        // UI phase 6, D3) — this placeholder is never observed.
        run_id: Uuid::new_v4(),
        kind,
        task,
        session_id,
        work_item_id: None,
        repo_path,
        depth,
    };
    crate::launch_coding_run(app, spec, resp_tx);
    Ok(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::queries::pipeline_runs::RunTransition;

    async fn git(dir: &std::path::Path, args: &[&str]) {
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .await
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    async fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-b", "main"]).await;
        git(dir.path(), &["config", "user.email", "t@haily.test"]).await;
        git(dir.path(), &["config", "user.name", "Test"]).await;
        tokio::fs::write(dir.path().join("README.md"), "hello\n")
            .await
            .unwrap();
        git(dir.path(), &["add", "."]).await;
        git(dir.path(), &["commit", "-m", "init"]).await;
        dir
    }

    async fn test_db() -> (tempfile::TempDir, DbHandle, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        let session_id = Uuid::new_v4().to_string();
        haily_db::queries::sessions::create_session(&db, &session_id, "coding", None)
            .await
            .unwrap();
        (dir, db, session_id)
    }

    /// A workspace whose driving run is still `queued`/`running` (no `run_id` stamped yet) must
    /// still surface as linked — found via the session fallback, not left run-less.
    #[tokio::test]
    async fn list_workspaces_finds_an_in_flight_run_by_session_when_run_id_is_unstamped() {
        let repo = init_repo().await;
        let (_dbdir, db, session_id) = test_db().await;
        let wt_root = tempfile::tempdir().unwrap();
        let ws = CodingWorkspace::open(&db, &session_id, repo.path(), wt_root.path(), None)
            .await
            .unwrap();
        assert!(ws.row.run_id.is_none(), "fixture: run_id not yet stamped");
        let run = pipeline_runs::create(&db, &session_id, None, 5)
            .await
            .unwrap();

        let views = list_workspaces(&db).await.unwrap();
        let view = views.iter().find(|v| v.id == ws.row.id).unwrap();
        assert_eq!(view.run_id.as_deref(), Some(run.id.as_str()));
        assert_eq!(view.run_status.as_deref(), Some("queued"));
        assert!(!view.resumable, "a live run is never resumable");
        assert!(!view.worktree_reclaimed);
    }

    /// An interrupted run with an intact worktree is resumable; once the worktree directory is
    /// gone the row must flip to reclaimed and NOT resumable — the exact guard `resume_run`
    /// itself enforces, never re-derived from status alone.
    #[tokio::test]
    async fn list_workspaces_resumable_flag_tracks_worktree_existence() {
        let repo = init_repo().await;
        let (_dbdir, db, session_id) = test_db().await;
        let wt_root = tempfile::tempdir().unwrap();
        let ws = CodingWorkspace::open(&db, &session_id, repo.path(), wt_root.path(), None)
            .await
            .unwrap();
        let run = pipeline_runs::create(&db, &session_id, None, 5)
            .await
            .unwrap();
        pipeline_runs::transition(
            &db,
            &run.id,
            RunTransition {
                stage_index: 0,
                status: "interrupted",
                attempt: 0,
                attempts_remaining: 5,
                tier_used: None,
                backend_used: None,
                egress: None,
                gate_output_digest: None,
                pause_reason_class: None,
            },
        )
        .await
        .unwrap();
        coding_workspaces::set_run_id(&db, &ws.row.id, &run.id)
            .await
            .unwrap();

        let views = list_workspaces(&db).await.unwrap();
        let view = views.iter().find(|v| v.id == ws.row.id).unwrap();
        assert!(
            view.resumable,
            "interrupted + intact worktree must be resumable"
        );
        assert!(!view.worktree_reclaimed);

        tokio::fs::remove_dir_all(ws.worktree_root()).await.unwrap();

        let views = list_workspaces(&db).await.unwrap();
        let view = views.iter().find(|v| v.id == ws.row.id).unwrap();
        assert!(
            !view.resumable,
            "a reclaimed worktree must never be offered for Continue"
        );
        assert!(view.worktree_reclaimed);
        assert_eq!(view.changed_file_count, 0);
    }
}
