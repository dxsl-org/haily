use haily_core::worktree::EphemeralWorktree;
use std::process::Command;
use tempfile::TempDir;

/// Create a minimal git repo with one empty commit so HEAD is valid.
fn init_git_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = dir.path();

    let init = Command::new("git")
        .args(["init"])
        .current_dir(p)
        .output()
        .expect("git init");
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let commit = Command::new("git")
        .args([
            "-c",
            "user.email=test@haily.test",
            "-c",
            "user.name=Test",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ])
        .current_dir(p)
        .output()
        .expect("git commit");
    assert!(
        commit.status.success(),
        "initial commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );

    dir
}

#[tokio::test]
async fn new_creates_worktree_directory() {
    let repo = init_git_repo();
    let wt = EphemeralWorktree::new(repo.path()).await.unwrap();

    assert!(wt.path.exists(), "worktree checkout dir must exist");
    assert!(wt.path.is_dir(), "worktree checkout must be a directory");
}

#[tokio::test]
async fn cleanup_removes_directory() {
    let repo = init_git_repo();
    let wt = EphemeralWorktree::new(repo.path()).await.unwrap();
    let wt_path = wt.path.clone();

    assert!(wt_path.exists());
    wt.cleanup().await.unwrap();
    assert!(
        !wt_path.exists(),
        "cleanup must remove the worktree directory"
    );
}

#[tokio::test]
async fn cleanup_is_idempotent() {
    let repo = init_git_repo();
    let wt = EphemeralWorktree::new(repo.path()).await.unwrap();
    wt.cleanup().await.unwrap();
    // Second call: directory is already gone — should not error.
    wt.cleanup().await.unwrap();
}

#[tokio::test]
async fn diff_empty_on_clean_worktree() {
    let repo = init_git_repo();
    let wt = EphemeralWorktree::new(repo.path()).await.unwrap();

    let diff = wt.diff().await.unwrap();
    assert!(
        diff.is_empty(),
        "clean worktree must produce no diff, got:\n{diff}"
    );

    wt.cleanup().await.unwrap();
}

#[tokio::test]
async fn diff_captures_untracked_file() {
    let repo = init_git_repo();
    let wt = EphemeralWorktree::new(repo.path()).await.unwrap();

    tokio::fs::write(wt.path.join("hello.txt"), "world\n")
        .await
        .unwrap();

    let diff = wt.diff().await.unwrap();
    assert!(
        diff.contains("hello.txt"),
        "diff must mention the new file:\n{diff}"
    );
    assert!(
        diff.contains("+world"),
        "diff must show the file content:\n{diff}"
    );

    wt.cleanup().await.unwrap();
}

#[tokio::test]
async fn new_fails_for_non_git_directory() {
    let dir = tempfile::tempdir().unwrap();
    let result = EphemeralWorktree::new(dir.path()).await;
    assert!(result.is_err(), "must fail when .git is absent");
}

#[tokio::test]
async fn with_ephemeral_worktree_returns_value_and_diff() {
    let repo = init_git_repo();

    let (value, diff) =
        EphemeralWorktree::with_ephemeral_worktree(repo.path(), |wt_path| async move {
            tokio::fs::write(wt_path.join("output.txt"), "generated\n").await?;
            Ok::<i32, anyhow::Error>(99)
        })
        .await
        .unwrap();

    assert_eq!(value, 99);
    assert!(
        diff.contains("output.txt"),
        "diff must capture file written in closure:\n{diff}"
    );
}

#[tokio::test]
async fn with_ephemeral_worktree_cleans_up_on_completion() {
    let repo = init_git_repo();
    let mut captured_path = None;

    let (_, _) = EphemeralWorktree::with_ephemeral_worktree(repo.path(), |wt_path| {
        captured_path = Some(wt_path.clone());
        async move { Ok::<(), anyhow::Error>(()) }
    })
    .await
    .unwrap();

    if let Some(p) = captured_path {
        assert!(
            !p.exists(),
            "worktree must be removed after with_ephemeral_worktree completes"
        );
    }
}
