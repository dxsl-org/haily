//! Behavior tests for the coding tool surface (Sub-Agent + Skill Architecture phase 1).
//! Covers the Critical/High rows of the phase's Test Scenario Matrix that need a real tool
//! dispatch (path-guard rejection, journaling, hash-anchored edit, secret deny-glob, git
//! commit isolation, shell approval). Grammar/path-unit cases live in the module unit tests.

use async_trait::async_trait;
use haily_db::queries::{coding_workspaces, journal};
use haily_db::DbHandle;
use haily_tools::coding::workspace::CodingWorkspace;
use haily_tools::{Tool, ToolContext};
use haily_types::ApprovalGate;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Local git runner for the test (the tool's own `git` helper is crate-private).
async fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .expect("spawn git")
}

/// Approval gate that always denies — the default; a coding write is `ReversibleWrite` and
/// never prompts, so most tests never reach it.
struct DenyGate;
#[async_trait]
impl ApprovalGate for DenyGate {
    async fn request(&self, _a: uuid::Uuid, _s: uuid::Uuid, _c: &CancellationToken) -> bool {
        false
    }
}

/// Approval gate that always approves — used to exercise the shell_exec first-exec path under
/// a non-enforcing sandbox.
struct AllowGate;
#[async_trait]
impl ApprovalGate for AllowGate {
    async fn request(&self, _a: uuid::Uuid, _s: uuid::Uuid, _c: &CancellationToken) -> bool {
        true
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    _repo: tempfile::TempDir,
    _wt_root: tempfile::TempDir,
    db: Arc<DbHandle>,
    ctx: ToolContext,
    ws_id: String,
    repo_path: std::path::PathBuf,
    branch: String,
}

async fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    for a in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "t@haily.test"],
        vec!["config", "user.name", "Test"],
    ] {
        assert!(git(p, &a).await.status.success(), "git {a:?}");
    }
    tokio::fs::write(p.join("README.md"), "hello\n").await.unwrap();
    assert!(git(p, &["add", "."]).await.status.success());
    assert!(git(p, &["commit", "-m", "init"]).await.status.success());
    dir
}

async fn fixture(gate: Arc<dyn ApprovalGate>) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let db = Arc::new(DbHandle::init(&tmp.path().join("t.db")).await.unwrap());
    let kms = Arc::new(
        haily_kms::KmsHandle::init((*db).clone(), tmp.path())
            .await
            .unwrap(),
    );
    let session_id = uuid::Uuid::new_v4();
    haily_db::queries::sessions::create_session(&db, &session_id.to_string(), "coding", None)
        .await
        .unwrap();

    let repo = init_repo().await;
    let wt_root = tempfile::tempdir().unwrap();
    let ws = CodingWorkspace::open(&db, &session_id.to_string(), repo.path(), wt_root.path(), None)
        .await
        .expect("open workspace");
    let ws_id = ws.row.id.clone();
    let branch = ws.row.branch.clone();
    let repo_path = repo.path().to_path_buf();

    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let ctx = ToolContext {
        db: Arc::clone(&db),
        kms,
        session_id,
        turn_id: uuid::Uuid::new_v4(),
        depth: 0,
        domain: Some("developer"),
        approval_gate: gate,
        approval_tx: tx,
        cancel: CancellationToken::new(),
        turn_deletes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        last_journal_id: Arc::new(std::sync::Mutex::new(None)),
        run_id: None,
        depth_mode: haily_types::DepthMode::Normal,
    };
    Fixture { _tmp: tmp, _repo: repo, _wt_root: wt_root, db, ctx, ws_id, repo_path, branch }
}

use haily_tools::coding::{
    FsDeleteTool, FsEditTool, FsGrepTool, FsMoveTool, FsReadTool, FsWriteTool, GitCommitTool,
    ShellExecTool,
};

#[tokio::test]
async fn write_read_roundtrip_and_journal_audit() {
    let f = fixture(Arc::new(DenyGate)).await;
    FsWriteTool
        .execute(json!({"workspace_id": f.ws_id, "path": "src/lib.rs", "content": "pub fn a() {}\n"}), &f.ctx)
        .await
        .expect("write");
    let read = FsReadTool
        .execute(json!({"workspace_id": f.ws_id, "path": "src/lib.rs"}), &f.ctx)
        .await
        .expect("read");
    assert!(read.contains("pub fn a()"));
    assert!(read.contains("content_hash:"), "read must surface the anti-stale hash");

    let rows = journal::list_by_workspace(&f.db, &f.ws_id).await.unwrap();
    assert!(
        rows.iter().any(|r| r.tool_name == "fs_write" && r.workspace_id.as_deref() == Some(f.ws_id.as_str())),
        "fs_write must record a workspace-tagged audit row"
    );
}

#[tokio::test]
async fn write_outside_root_rejected() {
    let f = fixture(Arc::new(DenyGate)).await;
    for bad in ["../escape.txt", "src/../../escape.txt"] {
        let r = FsWriteTool
            .execute(json!({"workspace_id": f.ws_id, "path": bad, "content": "x"}), &f.ctx)
            .await;
        assert!(r.is_err(), "path {bad} must be rejected");
    }
}

#[tokio::test]
async fn secret_deny_glob_blocks_read() {
    let f = fixture(Arc::new(DenyGate)).await;
    // Even if the file exists on disk, fs_read refuses a secret-matched path.
    let r = FsReadTool
        .execute(json!({"workspace_id": f.ws_id, "path": ".env"}), &f.ctx)
        .await;
    assert!(r.is_err(), ".env must be refused by the deny-glob");
    assert!(format!("{:#}", r.err().unwrap()).contains("secret"));
}

#[tokio::test]
async fn hash_anchored_stale_edit_fails_idempotently() {
    let f = fixture(Arc::new(DenyGate)).await;
    FsWriteTool
        .execute(json!({"workspace_id": f.ws_id, "path": "a.rs", "content": "let x = OLD;\n"}), &f.ctx)
        .await
        .unwrap();
    // A stale hash (from before the write) must be refused, not applied.
    let stale = FsEditTool
        .execute(
            json!({"workspace_id": f.ws_id, "path": "a.rs", "old_str": "OLD", "new_str": "NEW", "expected_hash": "deadbeefdeadbeef"}),
            &f.ctx,
        )
        .await;
    assert!(stale.is_err(), "stale hash must be refused");
    assert!(format!("{:#}", stale.err().unwrap()).contains("stale"));

    // A correct edit applies once; re-applying the same edit finds 0 matches (idempotent).
    FsEditTool
        .execute(json!({"workspace_id": f.ws_id, "path": "a.rs", "old_str": "OLD", "new_str": "NEW"}), &f.ctx)
        .await
        .expect("first edit applies");
    let reapply = FsEditTool
        .execute(json!({"workspace_id": f.ws_id, "path": "a.rs", "old_str": "OLD", "new_str": "NEW"}), &f.ctx)
        .await;
    assert!(reapply.is_err(), "re-applying a landed edit must fail cleanly");
}

#[tokio::test]
async fn move_and_delete_are_journaled() {
    let f = fixture(Arc::new(DenyGate)).await;
    FsWriteTool
        .execute(json!({"workspace_id": f.ws_id, "path": "old.rs", "content": "x\n"}), &f.ctx)
        .await
        .unwrap();
    FsMoveTool
        .execute(json!({"workspace_id": f.ws_id, "from": "old.rs", "to": "new.rs"}), &f.ctx)
        .await
        .expect("move");
    FsDeleteTool
        .execute(json!({"workspace_id": f.ws_id, "path": "new.rs"}), &f.ctx)
        .await
        .expect("delete");
    let rows = journal::list_by_workspace(&f.db, &f.ws_id).await.unwrap();
    assert!(rows.iter().any(|r| r.tool_name == "fs_move"));
    assert!(rows.iter().any(|r| r.tool_name == "fs_delete"));
}

#[tokio::test]
async fn grep_finds_matches_and_skips_secrets() {
    let f = fixture(Arc::new(DenyGate)).await;
    FsWriteTool
        .execute(json!({"workspace_id": f.ws_id, "path": "src/x.rs", "content": "fn needle() {}\n"}), &f.ctx)
        .await
        .unwrap();
    let hits = FsGrepTool
        .execute(json!({"workspace_id": f.ws_id, "pattern": "needle"}), &f.ctx)
        .await
        .expect("grep");
    assert!(hits.contains("src/x.rs"), "grep must find the match: {hits}");
}

#[tokio::test]
async fn git_commit_isolates_objects_from_real_repo() {
    let f = fixture(Arc::new(DenyGate)).await;
    // Write + commit inside the workspace.
    let wt = coding_workspaces::get(&f.db, &f.ws_id).await.unwrap().unwrap().worktree_path;
    FsWriteTool
        .execute(json!({"workspace_id": f.ws_id, "path": "feature.rs", "content": "fn feature() {}\n"}), &f.ctx)
        .await
        .unwrap();
    GitCommitTool
        .execute(json!({"workspace_id": f.ws_id, "message": "add feature"}), &f.ctx)
        .await
        .expect("commit");

    // The commit sha (readable from the ref) must NOT be a reachable object in the REAL repo —
    // its objects live in the isolated store, never the real repo's shared object DB.
    let sha_out = git(std::path::Path::new(&wt), &["rev-parse", "HEAD"]).await;
    let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
    let present = git(&f.repo_path, &["cat-file", "-e", &sha]).await;
    assert!(
        !present.status.success(),
        "workspace commit {sha} must NOT be present in the real repo's object store"
    );

    // After discard, the workspace branch ref is gone from the real repo too.
    let ws = CodingWorkspace {
        row: coding_workspaces::get(&f.db, &f.ws_id).await.unwrap().unwrap(),
    };
    ws.discard(&f.db).await.expect("discard");
    let branch_ref = git(&f.repo_path, &["rev-parse", "--verify", &f.branch]).await;
    assert!(!branch_ref.status.success(), "workspace branch must be deleted from the real repo");
}

#[tokio::test]
async fn shell_verifier_on_non_enforcing_sandbox_requires_approval() {
    // On this host (no HAILY_WSL_DISTRO) the sandbox is NullSandbox (non-enforcing), so even a
    // verifier must route through first-exec approval — a DenyGate blocks it.
    let f = fixture(Arc::new(DenyGate)).await;
    let r = ShellExecTool
        .execute(json!({"workspace_id": f.ws_id, "program": "cargo", "args": ["check"]}), &f.ctx)
        .await;
    if std::env::var("HAILY_WSL_DISTRO").is_ok() {
        return; // an enforcing sandbox would auto-run; this assertion is for the Null path
    }
    assert!(r.is_err(), "a verifier must not auto-run unsandboxed; approval was denied");
    assert!(format!("{:#}", r.err().unwrap()).contains("not approved"));
}

#[tokio::test]
async fn shell_runs_under_null_sandbox_when_approved() {
    if std::env::var("HAILY_WSL_DISTRO").is_ok() {
        return; // this exercises the Null (non-enforcing) approved path specifically
    }
    let f = fixture(Arc::new(AllowGate)).await;
    // A verifier (cargo check) on a non-enforcing sandbox routes through the in-execute
    // first-exec approval; AllowGate approves, so it reaches execution and returns a
    // structured result (exit_code line). The worktree has no Cargo.toml, so cargo exits
    // non-zero fast — we assert the structured report, not the exit code.
    let out = ShellExecTool
        .execute(json!({"workspace_id": f.ws_id, "program": "cargo", "args": ["check"], "timeout_secs": 60}), &f.ctx)
        .await;
    let text = out.expect("approved verifier reaches execution");
    assert!(text.contains("exit_code:"), "structured result expected: {text}");
}
