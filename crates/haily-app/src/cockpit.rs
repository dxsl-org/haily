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
//! skill from injection, prioritizing a pinned one — is deferred to the P11b GUI-wiring PR,
//! mirroring the established `set_connector_status` precedent in this codebase (an admin
//! toggle whose persisted state the runtime consumes at a later, documented point), rather
//! than threading a pref read into the two hot injection paths (sync in-memory authored +
//! async DB synthesized) in this backbone PR.

use anyhow::Result;
use haily_db::queries::{coding_workspaces, meta, skills as db_skills};
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_tools::coding::workspace::CodingWorkspace;
use haily_tools::exec::{Manager, SandboxKind};
use serde::Serialize;

/// Preference key prefixes for per-skill enable/pin state (see the module doc).
const SKILL_ENABLED_PREFIX: &str = "skill.enabled.";
const SKILL_PINNED_PREFIX: &str = "skill.pinned.";

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

/// One active coding workspace for the workspace/sandbox panel.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceView {
    pub id: String,
    pub session_id: String,
    pub repo_path: String,
    pub branch: String,
    pub worktree_path: String,
    pub work_item_id: Option<String>,
    pub created_at: String,
    /// Whether the worktree has uncommitted changes (`git status --porcelain`). `false` on
    /// a status-probe failure (logged) rather than blocking the whole list.
    pub dirty: bool,
    /// The host's active sandbox backend name (`"wsl2"`/`"null"`/…). Host-global, repeated
    /// per row so a panel can badge each workspace.
    pub sandbox_kind: String,
    /// `false` for the non-enforcing `NullSandbox` — the signal the panel must surface as a
    /// "first-exec approval required, not isolated" warning.
    pub sandbox_enforcing: bool,
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
    let enabled_prefs = meta::list_by_prefix(db, SKILL_ENABLED_PREFIX).await.unwrap_or_default();
    let pinned_prefs = meta::list_by_prefix(db, SKILL_PINNED_PREFIX).await.unwrap_or_default();

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

/// List active coding workspaces with per-workspace dirty status and the host sandbox
/// posture (phase 11a). The sandbox kind is probed ONCE (host-global) and stamped on each
/// row; dirty is probed per workspace and fails soft to `false` on a git error (logged),
/// so one broken worktree never blanks the whole panel.
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
        let dirty = match ws.is_dirty().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(workspace = %ws.row.id, "dirty probe failed, defaulting to false: {e:#}");
                false
            }
        };
        out.push(WorkspaceView {
            id: ws.row.id.clone(),
            session_id: ws.row.session_id.clone(),
            repo_path: ws.row.repo_path.clone(),
            branch: ws.row.branch.clone(),
            worktree_path: ws.row.worktree_path.clone(),
            work_item_id: ws.row.work_item_id.clone(),
            created_at: ws.row.created_at.clone(),
            dirty,
            sandbox_kind: sandbox_kind.clone(),
            sandbox_enforcing,
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
