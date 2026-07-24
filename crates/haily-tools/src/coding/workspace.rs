//! `CodingWorkspace` — a dedicated git worktree of a target repo, the single authoritative
//! compensator for every in-workspace file change.
//!
//! Undo of any coding change is a worktree reset: `git checkout -- .` (revert tracked) +
//! `git clean -ffdx` (remove untracked AND gitignored artifacts). The `-x` and `-ff` flags
//! are LOAD-BEARING (P0 spike finding U1): a bare `git clean -fd` leaves gitignored
//! `target/`/`node_modules/` behind, so the workspace would not be truly reverted. Safe
//! because the worktree is ephemeral. Journal rows are audit/display only — never the
//! rollback source of truth (red-team FMA-C2: two compensators over the same bytes is a bug).
//!
//! git isolation: workspace commits write objects to an isolated store sibling to the
//! worktree (`GIT_OBJECT_DIRECTORY`), never the real repo's shared object DB, so discarding
//! the worktree leaves no reachable objects (or reflog churn) in the user's repo.

use anyhow::{bail, Context, Result};
use haily_db::queries::coding_workspaces::{self, CodingWorkspaceRow};
use haily_db::DbHandle;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::process::Command;

/// Suffix of the isolated per-workspace object store, a sibling dir of the worktree so it
/// is not itself swept by the worktree's own `git clean -ffdx`.
const OBJ_DIR_SUFFIX: &str = ".git-objects";

pub struct CodingWorkspace {
    pub row: CodingWorkspaceRow,
}

/// See [`CodingWorkspace::change_summary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceChangeSummary {
    pub changed_file_count: usize,
    pub dirty: bool,
}

impl CodingWorkspace {
    /// Open a fresh workspace: cut a new git worktree (+ ephemeral branch) of `repo_path`
    /// under `worktrees_root`, and persist the lifecycle row.
    ///
    /// **Fails loud if `repo_path` is not a git repository** (red-team AD-C3: a non-git
    /// target cannot get a workspace — a documented precondition, not a silent no-op).
    ///
    /// # Errors
    /// Returns an error if the target is not a git repo, `git worktree add` fails, or the
    /// row insert fails.
    pub async fn open(
        db: &DbHandle,
        session_id: &str,
        repo_path: &Path,
        worktrees_root: &Path,
        work_item_id: Option<&str>,
    ) -> Result<Self> {
        ensure_git_repo(repo_path).await?;

        let id = coding_workspaces::new_id();
        let branch = format!("haily/ws-{}", &id[..8.min(id.len())]);
        let worktree_path = worktrees_root.join(&id);
        tokio::fs::create_dir_all(worktrees_root)
            .await
            .context("creating worktrees root")?;

        let wt_str = worktree_path
            .to_str()
            .context("worktree path is not valid UTF-8")?;
        let repo_str = repo_path.to_str().context("repo path is not valid UTF-8")?;

        let out = git(
            repo_path,
            &["worktree", "add", "-b", &branch, wt_str, "HEAD"],
            &[],
        )
        .await?;
        if !out.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let row =
            coding_workspaces::create(db, &id, session_id, repo_str, &branch, wt_str, work_item_id)
                .await?;
        Ok(Self { row })
    }

    /// The canonicalizable worktree root.
    pub fn worktree_root(&self) -> &Path {
        Path::new(&self.row.worktree_path)
    }

    /// The isolated object-store dir used for workspace commits (sibling of the worktree).
    pub fn object_dir(&self) -> PathBuf {
        PathBuf::from(object_dir_for(&self.row.worktree_path))
    }

    /// Resolve the isolated-object env pair `(GIT_OBJECT_DIRECTORY, GIT_ALTERNATE_OBJECT_DIRECTORIES)`
    /// every git op against this workspace uses (P4b review fix). `commit_stage` WRITES new
    /// commit objects into the isolated dir (never the real repo's store); `compensate` must
    /// resolve those objects to reset the working tree, so it uses the SAME pair — an empty
    /// isolated dir (before any commit) is a harmless primary object dir, since every read falls
    /// through to the alternate (the real repo's objects). Creates the isolated dir if absent.
    ///
    /// # Errors
    /// Returns an error if resolving the repo's git-path objects fails or the isolated dir
    /// cannot be created.
    async fn isolated_git_env(&self) -> Result<(String, String)> {
        let repo = Path::new(&self.row.repo_path);
        let alt_out = git(repo, &["rev-parse", "--git-path", "objects"], &[]).await?;
        if !alt_out.status.success() {
            bail!(
                "cannot resolve repo objects dir: {}",
                String::from_utf8_lossy(&alt_out.stderr)
            );
        }
        let alt_rel = String::from_utf8_lossy(&alt_out.stdout).trim().to_string();
        let alt_abs = repo.join(&alt_rel);
        let obj_dir = self.object_dir();
        tokio::fs::create_dir_all(&obj_dir)
            .await
            .context("creating isolated object dir")?;
        let obj_dir_str = obj_dir
            .to_str()
            .context("object dir path is not valid UTF-8")?
            .to_string();
        let alt_abs_str = alt_abs
            .to_str()
            .context("alternate objects path is not valid UTF-8")?
            .to_string();
        Ok((obj_dir_str, alt_abs_str))
    }

    /// Revert ALL in-workspace changes (tracked reverted, untracked+ignored removed) back to
    /// CURRENT HEAD. Leaves the worktree registered so a later op can reuse it. This is the
    /// compensator invoked to roll a workspace back to its entry state — "entry" meaning
    /// whatever HEAD is at call time, which [`Self::commit_stage`] advances at each passed
    /// pipeline stage boundary (P4b review fix: without a stage-boundary commit, HEAD stays at
    /// the RUN's entry forever, so a later stage's retry-reset would wipe every earlier PASSED
    /// stage's output, not just the failing stage's).
    ///
    /// # Errors
    /// Returns an error if resolving the isolated git env or either git step fails.
    pub async fn compensate(&self) -> Result<()> {
        let root = self.worktree_root();
        let (obj_dir, alt_dir) = self.isolated_git_env().await?;
        let env = [
            ("GIT_OBJECT_DIRECTORY", obj_dir.as_str()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alt_dir.as_str()),
        ];
        let co = git(root, &["checkout", "--", "."], &env).await?;
        if !co.status.success() {
            bail!(
                "git checkout -- . failed: {}",
                String::from_utf8_lossy(&co.stderr)
            );
        }
        let clean = git(root, &["clean", "-ffdx"], &env).await?;
        if !clean.status.success() {
            bail!(
                "git clean -ffdx failed: {}",
                String::from_utf8_lossy(&clean.stderr)
            );
        }
        Ok(())
    }

    /// Commit ALL current worktree changes onto the workspace's own ephemeral branch, in the
    /// SAME isolated object store `git_commit` (the LLM-facing tool in `git_tools.rs`) uses —
    /// new commit/tree/blob objects never enter the real repo's shared object DB. Called by the
    /// pipeline runner (P4b review fix, FMA-M3) after each stage PASSES its gate, so a LATER
    /// stage's retry-triggered [`Self::compensate`] resets to THIS stage's entry point, not the
    /// whole run's. A no-op `Ok(())` when there is nothing to commit (a stage that made no file
    /// changes) — `git commit` would otherwise fail on an empty commit.
    ///
    /// # Errors
    /// Returns an error if resolving the isolated git env, `git add -A`, or `git commit` fails
    /// for any reason OTHER than "nothing to commit".
    pub async fn commit_stage(&self, message: &str) -> Result<()> {
        let root = self.worktree_root();
        let (obj_dir, alt_dir) = self.isolated_git_env().await?;
        let env = [
            ("GIT_OBJECT_DIRECTORY", obj_dir.as_str()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alt_dir.as_str()),
        ];
        let add = git(root, &["add", "-A"], &env).await?;
        if !add.status.success() {
            bail!("git add failed: {}", String::from_utf8_lossy(&add.stderr));
        }
        let commit = git(
            root,
            &[
                "-c",
                "user.name=Haily",
                "-c",
                "user.email=haily@localhost",
                "commit",
                "-m",
                message,
            ],
            &env,
        )
        .await?;
        if !commit.status.success() {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&commit.stderr),
                String::from_utf8_lossy(&commit.stdout)
            );
            if combined.contains("nothing to commit") {
                return Ok(());
            }
            bail!("git commit failed: {combined}");
        }
        Ok(())
    }

    /// Whether the worktree has any uncommitted change (tracked-modified, staged, or
    /// untracked), via `git status --porcelain` (phase 11a — the workspace panel's "dirty"
    /// dot). Uses the isolated object env so it agrees with `commit_stage`/`compensate`.
    ///
    /// # Errors
    /// Returns an error if resolving the isolated git env or the `git status` call fails.
    pub async fn is_dirty(&self) -> Result<bool> {
        let root = self.worktree_root();
        let (obj_dir, alt_dir) = self.isolated_git_env().await?;
        let env = [
            ("GIT_OBJECT_DIRECTORY", obj_dir.as_str()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alt_dir.as_str()),
        ];
        let out = git(root, &["status", "--porcelain"], &env).await?;
        if !out.status.success() {
            bail!(
                "git status --porcelain failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
    }

    /// On-disk change summary for the Workspaces screen (Unified Chat UI phase 10). `None`
    /// means the worktree DIRECTORY ITSELF no longer exists — already reclaimed by a completed
    /// `worktree_apply` (which force-removes it as the last step of a successful apply) or a
    /// crash-orphan GC — a state the caller must surface distinctly (e.g. "cleaned up"), never
    /// swallow as an ordinary probe failure the way [`Self::is_dirty`]'s callers historically
    /// have. `Some` uses the SAME isolated git env as `is_dirty`/`commit_stage` so a workspace
    /// with stage-committed history (objects living only in the isolated store) still resolves
    /// correctly; `changed_file_count` is one `git status --porcelain` line per changed path.
    ///
    /// # Errors
    /// Returns an error if resolving the isolated git env or the `git status` call fails for a
    /// reason OTHER than the worktree directory being absent.
    pub async fn change_summary(&self) -> Result<Option<WorkspaceChangeSummary>> {
        let root = self.worktree_root();
        let exists = tokio::fs::metadata(root)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if !exists {
            return Ok(None);
        }
        let (obj_dir, alt_dir) = self.isolated_git_env().await?;
        let env = [
            ("GIT_OBJECT_DIRECTORY", obj_dir.as_str()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alt_dir.as_str()),
        ];
        let out = git(root, &["status", "--porcelain"], &env).await?;
        if !out.status.success() {
            bail!(
                "git status --porcelain failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let changed_file_count = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        Ok(Some(WorkspaceChangeSummary {
            changed_file_count,
            dirty: changed_file_count > 0,
        }))
    }

    /// The worktree's current unified diff against HEAD (phase 11a — the cockpit
    /// DiffViewer's read side; the ACCEPT side routes through the existing `worktree_apply`
    /// approval, not this). Capped at `max_bytes` so a huge generated diff cannot flood the
    /// IPC channel — a truncated diff is marked with a trailing notice; deep per-file
    /// viewing/virtualization is the frontend's job. The caller MUST treat the result as
    /// inert, untrusted content (it is repo/tool-derived) and render it as data only.
    ///
    /// # Errors
    /// Returns an error if resolving the isolated git env or the `git diff` call fails.
    pub async fn unified_diff(&self, max_bytes: usize) -> Result<String> {
        let root = self.worktree_root();
        let (obj_dir, alt_dir) = self.isolated_git_env().await?;
        let env = [
            ("GIT_OBJECT_DIRECTORY", obj_dir.as_str()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alt_dir.as_str()),
        ];
        // Include untracked files so a newly-created file shows up in the review diff.
        let out = git(root, &["--no-pager", "diff", "HEAD", "--"], &env).await?;
        if !out.status.success() {
            bail!("git diff failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let diff = String::from_utf8_lossy(&out.stdout);
        if diff.len() <= max_bytes {
            return Ok(diff.into_owned());
        }
        // Truncate on a char boundary to avoid splitting a multibyte sequence.
        let mut end = max_bytes;
        while end > 0 && !diff.is_char_boundary(end) {
            end -= 1;
        }
        Ok(format!(
            "{}\n… [diff truncated at {max_bytes} bytes]\n",
            &diff[..end]
        ))
    }

    /// Full teardown: revert, remove the worktree + its isolated object store, delete the
    /// ephemeral branch, and soft-delete the row. Best-effort past the row delete so a
    /// half-torn-down workspace still ends `deleted_at`-marked.
    ///
    /// # Errors
    /// Returns an error only if the row soft-delete fails; git teardown failures are logged.
    pub async fn discard(&self, db: &DbHandle) -> Result<()> {
        if let Err(e) = self.compensate().await {
            tracing::warn!(workspace = %self.row.id, "compensate during discard failed: {e}");
        }
        let repo = Path::new(&self.row.repo_path);
        let _ = git(
            repo,
            &["worktree", "remove", "--force", &self.row.worktree_path],
            &[],
        )
        .await;
        let _ = git(repo, &["branch", "-D", &self.row.branch], &[]).await;
        let _ = tokio::fs::remove_dir_all(self.object_dir()).await;
        coding_workspaces::soft_delete(db, &self.row.id).await?;
        Ok(())
    }
}

/// The isolated per-workspace object-store path for a given worktree path (sibling dir, so it
/// escapes the worktree's own `git clean -ffdx`). Shared by `CodingWorkspace::object_dir` and
/// `git_commit`'s `GIT_OBJECT_DIRECTORY` so both agree on the location.
pub fn object_dir_for(worktree_path: &str) -> String {
    format!("{worktree_path}{OBJ_DIR_SUFFIX}")
}

/// Fail loud unless `repo_path` is inside a git work tree.
async fn ensure_git_repo(repo_path: &Path) -> Result<()> {
    if !repo_path.is_dir() {
        bail!(
            "target repo path does not exist or is not a directory: {}",
            repo_path.display()
        );
    }
    let out = git(repo_path, &["rev-parse", "--is-inside-work-tree"], &[]).await?;
    let ok = out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true";
    if !ok {
        bail!(
            "target is not a git repository (required for a coding workspace): {}",
            repo_path.display()
        );
    }
    Ok(())
}

/// A process-unique EMPTY directory used as `core.hooksPath` for every host git invocation.
/// A coding workspace's target repo is user-selected and may carry a pre-existing (local,
/// untracked) hook; without this, `git worktree add` (post-checkout), `git commit`
/// (pre-commit/commit-msg/post-commit), etc. would fire that hook ON THE HOST — outside the
/// P0 sandbox, with the inherited parent env — which is exactly the RCE class the sandbox
/// exists to contain. Created once, never populated, so it holds no hook files.
fn empty_hooks_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("haily-nohooks-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&d);
        d
    })
    .as_path()
}

/// Run `git -C <dir> <args>` with optional extra env pairs, capturing output. Uses argv
/// (never a shell string) so there is no interpolation surface. Every invocation pins
/// `core.hooksPath` at an empty dir so a hook in the (untrusted) target repo cannot fire on
/// the host — see [`empty_hooks_dir`].
pub(crate) async fn git(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<std::process::Output> {
    let mut cmd = Command::new("git");
    // Forward slashes: git accepts them in config values on Windows and avoids backslash-escape
    // ambiguity in the `-c` value.
    let hooks = empty_hooks_dir().display().to_string().replace('\\', "/");
    cmd.arg("-C")
        .arg(dir)
        .arg("-c")
        .arg(format!("core.hooksPath={hooks}"))
        .args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().await.context("spawning git")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@haily.test"],
            vec!["config", "user.name", "Test"],
        ] {
            let a: Vec<&str> = args;
            let out = git(p, &a, &[]).await.unwrap();
            assert!(
                out.status.success(),
                "git {a:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        tokio::fs::write(p.join("README.md"), "hello\n")
            .await
            .unwrap();
        let add = git(p, &["add", "."], &[]).await.unwrap();
        assert!(add.status.success());
        let commit = git(p, &["commit", "-m", "init"], &[]).await.unwrap();
        assert!(
            commit.status.success(),
            "{}",
            String::from_utf8_lossy(&commit.stderr)
        );
        dir
    }

    async fn db() -> (tempfile::TempDir, std::sync::Arc<DbHandle>, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());
        let session_id = uuid::Uuid::new_v4().to_string();
        haily_db::queries::sessions::create_session(&db, &session_id, "coding", None)
            .await
            .unwrap();
        (dir, db, session_id)
    }

    #[tokio::test]
    async fn open_non_git_target_fails_loud() {
        let (dbdir, db, sess) = db().await;
        let not_git = tempfile::tempdir().unwrap();
        let wt_root = tempfile::tempdir().unwrap();
        let r = CodingWorkspace::open(&db, &sess, not_git.path(), wt_root.path(), None).await;
        assert!(r.is_err(), "a non-git target must fail loud");
        assert!(format!("{:#}", r.err().unwrap()).contains("not a git repository"));
        drop(dbdir);
    }

    #[tokio::test]
    async fn open_creates_worktree_and_row() {
        let repo = init_repo().await;
        let (_dbdir, db, sess) = db().await;
        let wt_root = tempfile::tempdir().unwrap();
        let ws = CodingWorkspace::open(&db, &sess, repo.path(), wt_root.path(), None)
            .await
            .expect("open");
        assert!(ws.worktree_root().is_dir(), "worktree dir created");
        assert!(
            ws.worktree_root().join("README.md").is_file(),
            "checked out HEAD"
        );
        let row = coding_workspaces::get(&db, &ws.row.id).await.unwrap();
        assert!(row.is_some(), "row persisted");
    }

    #[tokio::test]
    async fn compensate_removes_untracked_and_gitignored_and_reverts_tracked() {
        let repo = init_repo().await;
        let (_dbdir, db, sess) = db().await;
        let wt_root = tempfile::tempdir().unwrap();
        let ws = CodingWorkspace::open(&db, &sess, repo.path(), wt_root.path(), None)
            .await
            .unwrap();
        let root = ws.worktree_root().to_path_buf();

        // Mutate tracked, add untracked, add a gitignored artifact dir.
        tokio::fs::write(root.join("README.md"), "TAMPERED\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("scratch.txt"), "junk\n")
            .await
            .unwrap();
        tokio::fs::write(root.join(".gitignore"), "target/\n")
            .await
            .unwrap();
        tokio::fs::create_dir_all(root.join("target"))
            .await
            .unwrap();
        tokio::fs::write(root.join("target").join("build.bin"), "artifact")
            .await
            .unwrap();

        ws.compensate().await.expect("compensate");

        // Tracked reverted, untracked gone, gitignored dir gone (the -x/-ff proof, U1).
        // Normalize line endings — git may check out CRLF on Windows (autocrlf).
        let readme = tokio::fs::read_to_string(root.join("README.md"))
            .await
            .unwrap();
        assert_eq!(readme.replace("\r\n", "\n"), "hello\n");
        assert!(
            !root.join("scratch.txt").exists(),
            "untracked file must be removed"
        );
        assert!(
            !root.join("target").exists(),
            "gitignored target/ must be removed (-x)"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn host_git_neutralizes_repo_hooks() {
        use std::os::unix::fs::PermissionsExt;
        // A pre-commit hook that would BLOCK any commit if it fired.
        let repo = init_repo().await;
        let hooks = repo.path().join(".git").join("hooks");
        tokio::fs::create_dir_all(&hooks).await.unwrap();
        let pre = hooks.join("pre-commit");
        tokio::fs::write(&pre, "#!/bin/sh\nexit 1\n").await.unwrap();
        let mut perm = std::fs::metadata(&pre).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&pre, perm).unwrap();

        tokio::fs::write(repo.path().join("f.txt"), "x")
            .await
            .unwrap();
        let add = git(repo.path(), &["add", "-A"], &[]).await.unwrap();
        assert!(add.status.success());
        let commit = git(
            repo.path(),
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "x",
            ],
            &[],
        )
        .await
        .unwrap();
        assert!(
            commit.status.success(),
            "host git must neutralize the repo's pre-commit hook (it would exit 1): {}",
            String::from_utf8_lossy(&commit.stderr)
        );
    }

    #[tokio::test]
    async fn change_summary_reports_none_when_the_worktree_directory_is_gone() {
        let repo = init_repo().await;
        let (_dbdir, db, sess) = db().await;
        let wt_root = tempfile::tempdir().unwrap();
        let ws = CodingWorkspace::open(&db, &sess, repo.path(), wt_root.path(), None)
            .await
            .unwrap();
        tokio::fs::remove_dir_all(ws.worktree_root()).await.unwrap();

        let summary = ws.change_summary().await.unwrap();
        assert!(
            summary.is_none(),
            "an absent worktree directory must report None, not an error or a false 'clean'"
        );
    }

    #[tokio::test]
    async fn change_summary_counts_changed_files_and_flags_dirty() {
        let repo = init_repo().await;
        let (_dbdir, db, sess) = db().await;
        let wt_root = tempfile::tempdir().unwrap();
        let ws = CodingWorkspace::open(&db, &sess, repo.path(), wt_root.path(), None)
            .await
            .unwrap();

        let clean = ws.change_summary().await.unwrap().unwrap();
        assert_eq!(clean.changed_file_count, 0);
        assert!(!clean.dirty);

        tokio::fs::write(ws.worktree_root().join("README.md"), "changed\n")
            .await
            .unwrap();
        tokio::fs::write(ws.worktree_root().join("new.txt"), "new\n")
            .await
            .unwrap();

        let dirty = ws.change_summary().await.unwrap().unwrap();
        assert_eq!(dirty.changed_file_count, 2);
        assert!(dirty.dirty);
    }

    #[tokio::test]
    async fn discard_soft_deletes_row_and_removes_worktree() {
        let repo = init_repo().await;
        let (_dbdir, db, sess) = db().await;
        let wt_root = tempfile::tempdir().unwrap();
        let ws = CodingWorkspace::open(&db, &sess, repo.path(), wt_root.path(), None)
            .await
            .unwrap();
        let wt = ws.worktree_root().to_path_buf();
        ws.discard(&db).await.expect("discard");
        assert!(!wt.exists(), "worktree dir removed");
        assert!(coding_workspaces::get(&db, &ws.row.id)
            .await
            .unwrap()
            .is_none());
    }
}
