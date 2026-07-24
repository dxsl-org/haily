//! Integration tests for `kill_run`/`resume_run` bootstrapped through a real `AppHandle` (mirrors
//! `trigger::tests`'s own convention) so `resume_run` exercises the real `ApprovalBroker`,
//! `CodingWorkspace`, and `Orchestrator::launch_coding_run` paths.
use super::*;
use crate::bootstrap::BootstrapOptions;
use crate::test_support::{cloud_config, spawn_slow_llm_server, MockAdapter};
use haily_db::queries::coding_workspaces;
use haily_io::Adapter;
use haily_tools::coding::workspace::CodingWorkspace;

async fn bootstrapped() -> (AppHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let adapter = MockAdapter::new();
    let handle = AppHandle::bootstrap(
        dir.path(),
        vec![adapter as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await
    .expect("bootstrap");
    (handle, dir)
}

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

/// `pipeline_runs.session_id` FKs to `sessions(id)` — every test row needs a real session first.
async fn new_session(db: &haily_db::DbHandle) -> String {
    let id = Uuid::new_v4().to_string();
    haily_db::queries::sessions::create_session(db, &id, "coding", None)
        .await
        .unwrap();
    id
}

// ---------------------------------------------------------------------------
// is_resumable — pure predicate, shared by `resume_run` and the Workspaces screen (phase 10)
// ---------------------------------------------------------------------------

#[test]
fn is_resumable_accepts_interrupted_and_the_two_resumable_pause_classes() {
    assert!(is_resumable("interrupted", None));
    assert!(is_resumable("paused", Some("retries_exhausted")));
    assert!(is_resumable("paused", Some("explicit_stop")));
}

#[test]
fn is_resumable_refuses_live_terminal_and_non_resumable_pause_classes() {
    for status in ["queued", "running", "done", "failed"] {
        assert!(!is_resumable(status, None));
    }
    for class in [None, Some("awaiting_approval"), Some("other")] {
        assert!(!is_resumable("paused", class));
    }
}

// ---------------------------------------------------------------------------
// kill_run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_run_cancels_the_registered_token_and_soft_deletes_the_active_row() {
    let (handle, _dir) = bootstrapped().await;
    let run = pipeline_runs::create(&handle.db, &new_session(&handle.db).await, None, 5)
        .await
        .unwrap();
    let registry = RunControlRegistry::new();
    let token = tokio_util::sync::CancellationToken::new();
    registry.register(
        &run.id,
        token.clone(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );

    let acted = kill_run(&handle.db, &registry, &run.id).await.unwrap();

    assert!(acted, "kill_run must report it acted");
    assert!(token.is_cancelled(), "the registered token must fire");
    assert!(
        pipeline_runs::get(&handle.db, &run.id)
            .await
            .unwrap()
            .is_none(),
        "the checkpoint fallback must soft-delete the still-active row"
    );
}

#[tokio::test]
async fn kill_run_on_an_unknown_id_is_a_safe_no_op() {
    let (handle, _dir) = bootstrapped().await;
    let registry = RunControlRegistry::new();
    let acted = kill_run(&handle.db, &registry, "unknown-run")
        .await
        .unwrap();
    assert!(!acted);
}

#[tokio::test]
async fn kill_run_on_an_already_terminal_row_never_soft_deletes_it() {
    let (handle, _dir) = bootstrapped().await;
    let run = pipeline_runs::create(&handle.db, &new_session(&handle.db).await, None, 5)
        .await
        .unwrap();
    pipeline_runs::transition(
        &handle.db,
        &run.id,
        pipeline_runs::RunTransition {
            stage_index: 0,
            status: "done",
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
    let registry = RunControlRegistry::new();

    let acted = kill_run(&handle.db, &registry, &run.id).await.unwrap();

    assert!(!acted, "a done run has nothing left to kill");
    assert!(
        pipeline_runs::get(&handle.db, &run.id)
            .await
            .unwrap()
            .is_some(),
        "kill_run must never retroactively delete a run's completed history"
    );
}

// ---------------------------------------------------------------------------
// resume_run — eligibility + refusal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_run_on_an_unknown_id_is_ok_false() {
    let (handle, _dir) = bootstrapped().await;
    assert!(!resume_run(&handle, "unknown-run").await.unwrap());
}

#[tokio::test]
async fn resume_run_refuses_a_live_or_terminal_status() {
    let (handle, _dir) = bootstrapped().await;
    for status in ["queued", "running", "done", "failed"] {
        let run = pipeline_runs::create(&handle.db, &new_session(&handle.db).await, None, 5)
            .await
            .unwrap();
        pipeline_runs::transition(
            &handle.db,
            &run.id,
            pipeline_runs::RunTransition {
                stage_index: 0,
                status,
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
        assert!(
            !resume_run(&handle, &run.id).await.unwrap(),
            "status {status} must never be resumable"
        );
    }
}

#[tokio::test]
async fn resume_run_refuses_an_approval_wait_or_unclassified_pause() {
    let (handle, _dir) = bootstrapped().await;
    for class in [None, Some("awaiting_approval"), Some("other")] {
        let run = pipeline_runs::create(&handle.db, &new_session(&handle.db).await, None, 5)
            .await
            .unwrap();
        pipeline_runs::transition(
            &handle.db,
            &run.id,
            pipeline_runs::RunTransition {
                stage_index: 0,
                status: "paused",
                attempt: 0,
                attempts_remaining: 5,
                tier_used: None,
                backend_used: None,
                egress: None,
                gate_output_digest: None,
                pause_reason_class: class,
            },
        )
        .await
        .unwrap();
        assert!(
            !resume_run(&handle, &run.id).await.unwrap(),
            "pause class {class:?} must never resolve through resume_run"
        );
    }
}

#[tokio::test]
async fn resume_run_refuses_an_interrupted_row_with_no_resume_context() {
    // A row created before this migration (or by a caller like the eval harness) has NULL
    // task/run_kind/depth — nothing to reconstruct a relaunch from.
    let (handle, _dir) = bootstrapped().await;
    let run = pipeline_runs::create(&handle.db, &new_session(&handle.db).await, None, 5)
        .await
        .unwrap();
    pipeline_runs::transition(
        &handle.db,
        &run.id,
        pipeline_runs::RunTransition {
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
    assert!(!resume_run(&handle, &run.id).await.unwrap());
}

/// Builds a resumable `interrupted` row WITH a real on-disk workspace bound to its session —
/// shared setup for the "worktree gone" refusal and the happy-path relaunch test.
async fn resumable_row_with_workspace(
    handle: &AppHandle,
    repo: &std::path::Path,
) -> (pipeline_runs::PipelineRun, CodingWorkspace) {
    let session_id = Uuid::new_v4();
    haily_db::queries::sessions::create_session(
        &handle.db,
        &session_id.to_string(),
        "coding",
        None,
    )
    .await
    .unwrap();
    let wt_root = tempfile::tempdir().unwrap();
    // Leaked deliberately: the workspace's own worktree dir must outlive this helper (it is
    // asserted on / driven by a relaunch after this function returns).
    let wt_root_path = Box::leak(Box::new(wt_root)).path().to_path_buf();
    let workspace = CodingWorkspace::open(
        &handle.db,
        &session_id.to_string(),
        repo,
        &wt_root_path,
        None,
    )
    .await
    .unwrap();

    let run = pipeline_runs::create_resumable(
        &handle.db,
        None,
        &session_id.to_string(),
        None,
        5,
        Some(pipeline_runs::ResumeCtx {
            task: "add a feature",
            run_kind: "build",
            depth: "normal",
        }),
    )
    .await
    .unwrap();
    pipeline_runs::transition(
        &handle.db,
        &run.id,
        pipeline_runs::RunTransition {
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
    let run = pipeline_runs::get(&handle.db, &run.id)
        .await
        .unwrap()
        .unwrap();
    (run, workspace)
}

#[tokio::test]
async fn resume_run_refuses_with_a_clear_error_when_the_worktree_is_already_gone() {
    // Unified Chat UI phase 6 (D3): this is BOTH the reaper-race guard AND the no-double-apply
    // guard — a vanished worktree with a still-active workspace row means a prior ship already
    // fully applied (or the reaper reclaimed it), and re-emitting must never be attempted.
    let (handle, _dir) = bootstrapped().await;
    let repo = init_repo().await;
    let (run, workspace) = resumable_row_with_workspace(&handle, repo.path()).await;

    tokio::fs::remove_dir_all(workspace.worktree_root())
        .await
        .unwrap();

    let err = resume_run(&handle, &run.id)
        .await
        .expect_err("must refuse, not silently no-op");
    assert!(
        format!("{err:#}").contains("workspace"),
        "must be a clear, user-facing message: {err:#}"
    );

    // The row must be left exactly as found — no partial reset.
    let after = pipeline_runs::get(&handle.db, &run.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, "interrupted");
}

#[tokio::test]
async fn resume_run_refuses_when_the_workspace_row_itself_is_gone() {
    let (handle, _dir) = bootstrapped().await;
    let repo = init_repo().await;
    let (run, workspace) = resumable_row_with_workspace(&handle, repo.path()).await;
    coding_workspaces::soft_delete(&handle.db, &workspace.row.id)
        .await
        .unwrap();

    assert!(
        !resume_run(&handle, &run.id).await.unwrap(),
        "no workspace row at all is a plain no-op, not an error"
    );
}

// ---------------------------------------------------------------------------
// resume_run — happy path: relaunches and re-registers under the SAME run_id.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resumed_run_relaunches_and_is_killable_via_its_token_under_the_same_run_id() {
    let (handle, _dir) = bootstrapped().await;
    // A slow LLM keeps the relaunched stage sub-turn in flight long enough to observe the
    // registry entry before the run naturally finishes.
    let base_url = spawn_slow_llm_server(std::time::Duration::from_secs(5)).await;
    handle.orchestrator.reload_llm(cloud_config(base_url)).await;

    let repo = init_repo().await;
    let (run, _workspace) = resumable_row_with_workspace(&handle, repo.path()).await;

    let resumed = resume_run(&handle, &run.id)
        .await
        .expect("resume must succeed");
    assert!(
        resumed,
        "an interrupted row with real resume context must relaunch"
    );

    // REVIEW FIX (HIGH, phase-06 review): assert ACTUAL pipeline progress under the SAME
    // run_id, not merely that a token got registered (registration is synchronous in
    // `spawn_launch`, before `run()` — a `pipeline_runs.id` collision inside `run()` would have
    // aborted the relaunch with an Err the OLD version of this test could never see). A
    // `StageStarted` event persisted under `run.id` proves the resumed pipeline actually
    // entered a stage of the SAME row this test resumed, not just that a token exists.
    let saw_stage_started = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let events = haily_db::queries::run_events::list_run_events(&handle.db, &run.id)
                .await
                .unwrap();
            if events
                .iter()
                .any(|e| matches!(e, haily_types::RunEvent::StageStarted { .. }))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        saw_stage_started.is_ok(),
        "the resumed run must actually reach a stage under its own run_id, not die on a \
         pipeline_runs.id collision inside PipelineRunner::run"
    );

    // Killable via `kill_run` (the FULL contract: token cancel + soft-delete checkpoint
    // fallback — NOT the bare registry token alone) — proves re-registration under the SAME
    // run_id. The token-cancel does NOT itself abort an in-flight non-streaming LLM completion
    // call (`sub_turn.rs`'s `complete_tiered` has no `cancel` select — only the SSE/streaming
    // path does; a pre-existing runner-architecture property, unrelated to this fix), so the
    // OBSERVABLE, timing-independent proof `kill_run` actually acted on THIS row is its
    // synchronous soft-delete, not waiting on the mid-request stage to notice cancellation.
    let acted = kill_run(&handle.db, &handle.run_control_registry(), &run.id)
        .await
        .expect("kill_run query must not fail");
    assert!(
        acted,
        "kill_run must report it acted on the resumed run's own row"
    );
    assert!(
        pipeline_runs::get(&handle.db, &run.id)
            .await
            .unwrap()
            .is_none(),
        "kill_run's soft-delete checkpoint fallback must land on the SAME row the resume \
         relaunched — proves the relaunch's internal pipeline_runs row truly is `run.id`, not a \
         second row created under a different id (the collision bug's failure mode)"
    );
}
