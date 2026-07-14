//! Integration tests for bootstrap + shutdown. See `test_support` for the mock
//! adapter and the hand-rolled slow-LLM HTTP responder shared across these tests.
use crate::bootstrap::{AppHandle, BootstrapOptions};
use crate::test_support::{
    cloud_config, spawn_slow_llm_server, spawn_streaming_llm_server, MockAdapter,
};
use haily_db::{queries::meta, DbHandle};
use haily_io::{Adapter, ResponseChunk};
use std::sync::Arc;

/// Critical: bootstrap with a mock adapter, send a request, and confirm a response
/// (`Complete` chunk) comes back through the same adapter — proves the dispatch loop
/// wiring (adapter → orchestrator → adapter) works end to end.
#[tokio::test]
async fn bootstrap_roundtrips_a_request_through_the_mock_adapter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base_url = spawn_slow_llm_server(std::time::Duration::ZERO).await;

    let adapter = MockAdapter::new();
    let handle = AppHandle::bootstrap(
        dir.path(),
        vec![adapter.clone() as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await
    .expect("bootstrap");

    // Real LLM config only reachable after bootstrap (KMS preferences start empty),
    // so reload with the test server's URL for this specific turn.
    handle.orchestrator.reload_llm(cloud_config(base_url)).await;

    let session_id = adapter.send("hello").await;

    // Poll for the Complete chunk rather than a fixed sleep — bounded by an overall
    // timeout so a wiring regression fails fast instead of hanging the suite.
    let chunks = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let chunks = adapter.chunks_for(session_id);
            if chunks.iter().any(|c| matches!(c, ResponseChunk::Complete)) {
                return chunks;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("turn did not complete in time");

    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, ResponseChunk::Text(t) if t.contains("mock completion"))),
        "expected the mock LLM's completion text to be delivered, got: {chunks:?}"
    );

    handle.shutdown(std::time::Duration::from_secs(5)).await;
}

/// Critical: after `shutdown()`, every tracked task (dispatch loop, watcher, daemon
/// loops, self-improvement workers) must have exited — none leaked running past the
/// call.
#[tokio::test]
async fn shutdown_drains_all_tracked_tasks_within_timeout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let adapter = MockAdapter::new();

    let handle = AppHandle::bootstrap(
        dir.path(),
        vec![adapter as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await
    .expect("bootstrap");

    assert!(
        handle.task_count() > 0,
        "bootstrap should have registered background tasks"
    );

    // shutdown() itself asserts internally (via TaskTracker::wait under a timeout);
    // reaching this line without hanging is the pass condition for "no task leaked".
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        handle.shutdown(std::time::Duration::from_secs(5)),
    )
    .await
    .expect("shutdown must not hang past its own timeout budget");
}

/// Critical: shutdown() during an in-flight turn blocks until the turn task returns —
/// proves the per-turn spawn (dispatch.rs) goes through the TaskTracker, not a bare
/// detached `tokio::spawn`. A 1.5s artificial LLM latency together with a bounded
/// shutdown timeout would fail this test if the per-turn task were untracked
/// (shutdown would return almost immediately instead of waiting ~1.5s).
#[tokio::test]
async fn shutdown_blocks_until_an_in_flight_turn_finishes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let turn_latency = std::time::Duration::from_millis(1500);
    let base_url = spawn_slow_llm_server(turn_latency).await;

    let adapter = MockAdapter::new();
    let handle = AppHandle::bootstrap(
        dir.path(),
        vec![adapter.clone() as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await
    .expect("bootstrap");

    handle.orchestrator.reload_llm(cloud_config(base_url)).await;
    let session_id = adapter.send("slow turn").await;

    // Give the request time to reach the orchestrator and start the (slow) LLM call
    // before triggering shutdown, so this genuinely races an in-flight turn rather
    // than one that hasn't started yet.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let shutdown_started = std::time::Instant::now();
    handle.shutdown(std::time::Duration::from_secs(5)).await;
    let shutdown_elapsed = shutdown_started.elapsed();

    assert!(
        shutdown_elapsed >= std::time::Duration::from_millis(1000),
        "shutdown returned after {shutdown_elapsed:?}, expected it to block for ~{turn_latency:?} \
         waiting on the in-flight turn — the per-turn task is not being tracked"
    );

    let chunks = adapter.chunks_for(session_id);
    assert!(
        chunks.iter().any(|c| matches!(c, ResponseChunk::Complete)),
        "the in-flight turn should have been allowed to finish and deliver Complete"
    );
}

/// Critical (phase-06): shutdown fired WHILE tokens are actively streaming (not just
/// before the first byte, as in `shutdown_blocks_until_an_in_flight_turn_finishes`)
/// must end the turn quickly rather than waiting for the full 10-token stream to
/// finish — proves the per-turn `CancellationToken` (derived from the shutdown root,
/// see `dispatch.rs`) actually reaches `stream_llm_response`'s `select!` and the
/// cloud SSE task's own cancellation check, not just gating pre-stream connect.
#[tokio::test]
async fn shutdown_mid_stream_ends_the_turn_quickly_not_after_full_completion() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 10 tokens at 300ms apart = ~3s to finish uninterrupted; cancelling after the
    // first couple of tokens must return well under that.
    let token_count = 10;
    let inter_token_delay = std::time::Duration::from_millis(300);
    let base_url = spawn_streaming_llm_server(token_count, inter_token_delay).await;

    let adapter = MockAdapter::new();
    let handle = AppHandle::bootstrap(
        dir.path(),
        vec![adapter.clone() as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await
    .expect("bootstrap");

    handle.orchestrator.reload_llm(cloud_config(base_url)).await;
    let session_id = adapter.send("stream then cancel").await;

    // Let a couple of tokens land before cancelling, so this genuinely interrupts an
    // in-progress stream rather than racing a turn that hasn't started receiving yet.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;

    let shutdown_started = std::time::Instant::now();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle.shutdown(std::time::Duration::from_secs(5)),
    )
    .await
    .expect("shutdown must not hang");
    let shutdown_elapsed = shutdown_started.elapsed();

    let full_stream_duration = inter_token_delay * token_count;
    assert!(
        shutdown_elapsed < full_stream_duration,
        "shutdown took {shutdown_elapsed:?}, expected well under the full \
         {full_stream_duration:?} uninterrupted stream duration — cancellation did not \
         reach the in-progress stream"
    );

    let chunks = adapter.chunks_for(session_id);
    assert!(
        chunks.iter().any(|c| matches!(c, ResponseChunk::Complete)),
        "a cancelled turn must still finalize with Complete, not leave the adapter hanging: {chunks:?}"
    );
    // Some tokens should have streamed live before the cancellation cut it short —
    // proves this exercised real incremental delivery, not a turn that never started.
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, ResponseChunk::Text(t) if t.contains("tok0"))),
        "expected at least the first streamed token to have reached the adapter: {chunks:?}"
    );
}

/// Critical: `AppHandle::cancel_turn` on an unknown session must return `false`
/// rather than panicking or affecting any other in-flight turn — this is the exact
/// call the Tauri `cancel_turn` command makes for a stale/already-finished session id.
#[tokio::test]
async fn cancel_turn_on_unknown_session_returns_false() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = AppHandle::bootstrap(
        dir.path(),
        vec![MockAdapter::new() as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await
    .expect("bootstrap");

    assert!(
        !handle.cancel_turn(uuid::Uuid::new_v4()),
        "no turn registered for this session — must return false"
    );

    handle.shutdown(std::time::Duration::from_secs(5)).await;
}

/// Critical: a registered turn's token must actually fire when `AppHandle::cancel_turn`
/// is called by session id — this is the plumbing the GUI's Stop button depends on.
/// Uses the slow-LLM server (delay before any response) so there is a window where the
/// turn is in flight and its token is registered but not yet cleaned up.
#[tokio::test]
async fn cancel_turn_fires_the_in_flight_turns_token() {
    let dir = tempfile::tempdir().expect("tempdir");
    let turn_latency = std::time::Duration::from_millis(1500);
    let base_url = spawn_slow_llm_server(turn_latency).await;

    let adapter = MockAdapter::new();
    let handle = AppHandle::bootstrap(
        dir.path(),
        vec![adapter.clone() as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await
    .expect("bootstrap");

    handle.orchestrator.reload_llm(cloud_config(base_url)).await;
    let session_id = adapter.send("please cancel me").await;

    // Give dispatch time to register the turn before cancelling — otherwise this
    // would race a turn that hasn't been registered into the TurnRegistry yet.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        handle.cancel_turn(session_id),
        "a turn was in flight for this session — cancel_turn must find and cancel it"
    );

    // The cancelled turn should still finalize quickly (well under the full
    // uninitiated 1.5s LLM latency) rather than hang, proving the fired token
    // actually reached the in-flight `process` call.
    let chunks = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let chunks = adapter.chunks_for(session_id);
            if chunks.iter().any(|c| matches!(c, ResponseChunk::Complete)) {
                return chunks;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancelled turn did not finalize in time");
    assert!(
        chunks.iter().any(|c| matches!(c, ResponseChunk::Complete)),
        "a cancelled turn must still finalize with Complete: {chunks:?}"
    );

    handle.shutdown(std::time::Duration::from_secs(5)).await;
}

/// Critical: the registry entry for a turn must be removed once that turn completes
/// normally (no Stop click involved) — otherwise `TurnRegistry` would grow unbounded
/// over the life of a long-running process, one stale entry per historical turn.
#[tokio::test]
async fn turn_registry_does_not_leak_entries_after_normal_completion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base_url = spawn_slow_llm_server(std::time::Duration::ZERO).await;

    let adapter = MockAdapter::new();
    let handle = AppHandle::bootstrap(
        dir.path(),
        vec![adapter.clone() as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await
    .expect("bootstrap");

    handle.orchestrator.reload_llm(cloud_config(base_url)).await;
    let session_id = adapter.send("finish normally").await;

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let chunks = adapter.chunks_for(session_id);
            if chunks.iter().any(|c| matches!(c, ResponseChunk::Complete)) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("turn did not complete in time");

    // Poll briefly: the turn task's own cleanup (dispatch.rs's `turns_clone.remove`)
    // runs just after the Complete chunk is forwarded, so there is a small window
    // where it hasn't executed yet.
    let removed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if !handle.cancel_turn(session_id) {
                return;
            }
            // cancel_turn() returning true here would mean the entry leaked past
            // completion; if that ever happens, there is nothing left to clean up on
            // a second call so this loop would spin forever — bounded by the timeout.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        removed.is_ok(),
        "turn registry entry for a completed turn was never removed — it leaked"
    );

    handle.shutdown(std::time::Duration::from_secs(5)).await;
}

/// High: CLI mode enables the proactive daemon (mode asymmetry fix — F6). Verified by
/// task count delta rather than log scraping: with the daemon enabled, bootstrap
/// registers strictly more tasks than with it disabled (watcher + dispatch only).
#[tokio::test]
async fn daemon_option_registers_additional_background_tasks() {
    let dir_with = tempfile::tempdir().expect("tempdir");
    let with_daemon = AppHandle::bootstrap(
        dir_with.path(),
        vec![MockAdapter::new() as Arc<dyn Adapter>],
        BootstrapOptions {
            enable_daemon: true,
            enable_watcher: true,
            attempt_keyring: true,
        },
    )
    .await
    .expect("bootstrap with daemon");
    let count_with = with_daemon.task_count();
    with_daemon
        .shutdown(std::time::Duration::from_secs(5))
        .await;

    let dir_without = tempfile::tempdir().expect("tempdir");
    let without_daemon = AppHandle::bootstrap(
        dir_without.path(),
        vec![MockAdapter::new() as Arc<dyn Adapter>],
        BootstrapOptions {
            enable_daemon: false,
            enable_watcher: true,
            attempt_keyring: true,
        },
    )
    .await
    .expect("bootstrap without daemon");
    let count_without = without_daemon.task_count();
    without_daemon
        .shutdown(std::time::Duration::from_secs(5))
        .await;

    assert!(
        count_with > count_without,
        "enabling the proactive daemon should register additional background tasks \
         (with={count_with}, without={count_without})"
    );
}

/// Critical (Phase 4): every adapter passed to `bootstrap` must have the tool-
/// approval resolver injected before it starts accepting requests — otherwise a
/// `ToolApprovalRequest` reaching that adapter would have no way to be answered.
#[tokio::test]
async fn bootstrap_injects_the_approval_resolver_into_every_adapter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let adapter = MockAdapter::new();

    let handle = AppHandle::bootstrap(
        dir.path(),
        vec![adapter.clone() as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await
    .expect("bootstrap");

    assert!(
        adapter.has_approval_resolver(),
        "bootstrap must call Adapter::set_approval_resolver on every registered adapter"
    );
    assert!(
        adapter.has_turn_canceller(),
        "bootstrap must call Adapter::set_turn_canceller on every registered adapter \
         (Mobile Thin-Client plan phase 3 amendment)"
    );

    handle.shutdown(std::time::Duration::from_secs(5)).await;
}

/// Critical (Phase 4): listing a destructive/exfil tool in the `auto_approve`
/// allowlist must fail bootstrap outright — never silently ignored, never
/// downgraded to a warning. Pre-seeds the KMS preference directly in the DB file
/// bootstrap will open, since there is no running `KmsHandle` before bootstrap.
#[tokio::test]
async fn auto_approve_listing_a_destructive_tool_fails_bootstrap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("haily.db");

    {
        // Opened and dropped before `bootstrap` so its own pool isn't contending
        // with this seed write (SQLite WAL mode tolerates it either way, but
        // sequencing avoids any doubt about which write lands first).
        let seed_db = DbHandle::init(&db_path).await.expect("seed db init");
        meta::upsert_preference(
            &seed_db,
            "approval.auto_approve",
            &serde_json::to_string(&["worktree_apply"]).unwrap(),
            "test",
        )
        .await
        .expect("seed preference");
    }

    let result = AppHandle::bootstrap(
        dir.path(),
        vec![MockAdapter::new() as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await;

    let err = result
        .err()
        .expect("bootstrap must fail when auto_approve lists a destructive tool");
    assert!(
        err.to_string().contains("worktree_apply"),
        "error should name the offending tool, got: {err:#}"
    );
}

/// Critical (Pipeline Activation & Wiring phase 3): `start_coding_run` must reject an
/// unrecognized `kind` string synchronously — never minting a session or touching the
/// adapter — since the Tauri command layer forwards this string verbatim from the GUI's
/// Plan/Build toggle with no validation of its own.
#[tokio::test]
async fn start_coding_run_rejects_an_unknown_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = AppHandle::bootstrap(
        dir.path(),
        vec![MockAdapter::new() as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await
    .expect("bootstrap");

    let result = crate::cockpit::start_coding_run(
        &handle,
        "sprint",
        "do something".to_string(),
        None,
        haily_types::DepthMode::Normal,
    );
    let err = result.expect_err("an unknown kind must be rejected");
    assert!(
        err.to_string().contains("sprint"),
        "error should name the offending kind, got: {err:#}"
    );

    handle.shutdown(std::time::Duration::from_secs(5)).await;
}

/// Critical: a valid `start_coding_run` call must bind its minted session to the GUI
/// adapter and forward the launch's terminal chunks through that exact binding — proves
/// the wiring end to end (bind → launch → resp_tx forwarder → `AdapterManager::deliver`)
/// without needing a real LLM or a full pipeline run. Pointing `repo_path` at a plain
/// (non-git) tempdir makes `CodingWorkspace::open` fail fast (`ensure_git_repo`), so the
/// launch reaches its documented setup-failure path (`ResponseChunk::Error` then
/// `Complete`) well before any LLM call would be attempted.
#[tokio::test]
async fn start_coding_run_binds_the_session_and_forwards_the_launch_failure() {
    let data_dir = tempfile::tempdir().expect("data tempdir");
    let non_git_repo = tempfile::tempdir().expect("non-git repo tempdir");

    // Registered under "gui" (not the default "mock") since `start_coding_run` binds its
    // minted session to that fixed adapter id, mirroring the real `GuiAdapter::id()`.
    let adapter = MockAdapter::with_id("gui");
    let handle = AppHandle::bootstrap(
        data_dir.path(),
        vec![adapter.clone() as Arc<dyn Adapter>],
        BootstrapOptions::default(),
    )
    .await
    .expect("bootstrap");

    let session_id = crate::cockpit::start_coding_run(
        &handle,
        "plan",
        "wire up start_coding_run".to_string(),
        Some(non_git_repo.path().to_path_buf()),
        haily_types::DepthMode::Normal,
    )
    .expect("a recognized kind with an explicit repo_path must not fail synchronously");

    let chunks = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let chunks = adapter.chunks_for(session_id);
            if chunks.iter().any(|c| matches!(c, ResponseChunk::Complete)) {
                return chunks;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the launched run's setup failure did not reach the adapter in time");

    assert!(
        chunks.iter().any(|c| matches!(c, ResponseChunk::Error(_))),
        "a non-git repo_path should surface as a ResponseChunk::Error on the bound \
         session, got: {chunks:?}"
    );

    handle.shutdown(std::time::Duration::from_secs(5)).await;
}
