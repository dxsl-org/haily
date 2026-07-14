//! Unit tests for `resolve` (pure, no I/O) plus integration tests for the confirm-gate + launch
//! flow, bootstrapped through the real `AppHandle` so `confirm_then_launch` exercises the actual
//! `ApprovalBroker` and `Orchestrator::launch_coding_run`/`process` paths.
use super::*;
use crate::bootstrap::{AppHandle, BootstrapOptions};
use crate::test_support::{cloud_config, spawn_slow_llm_server, MockAdapter};
use haily_io::Adapter;

fn make_request(message: &str, origin: RequestOrigin) -> Request {
    Request {
        session_id: Uuid::new_v4(),
        adapter_id: "mock".to_string(),
        message: message.to_string(),
        user_ref: None,
        depth: Default::default(),
        origin,
    }
}

// -- resolve(): slash routing -------------------------------------------------------

#[test]
fn slash_plan_with_task_launches_plan() {
    let req = make_request("/plan add dark mode", RequestOrigin::Chat);
    match resolve(&req) {
        TriggerAction::LaunchPlan(task) => assert_eq!(task, "add dark mode"),
        _ => panic!("expected LaunchPlan"),
    }
}

#[test]
fn slash_code_and_build_alias_launch_build() {
    for cmd in ["/code fix the login bug", "/build fix the login bug"] {
        let req = make_request(cmd, RequestOrigin::Chat);
        match resolve(&req) {
            TriggerAction::LaunchBuild(task) => assert_eq!(task, "fix the login bug"),
            _ => panic!("expected LaunchBuild for {cmd}"),
        }
    }
}

#[test]
fn slash_empty_arg_prompts_instead_of_launching() {
    let plan_prompt = resolve(&make_request("/plan", RequestOrigin::Chat));
    assert!(matches!(plan_prompt, TriggerAction::PromptTask(RunKind::Plan)));

    for cmd in ["/code", "/build"] {
        let prompt = resolve(&make_request(cmd, RequestOrigin::Chat));
        assert!(
            matches!(prompt, TriggerAction::PromptTask(RunKind::Build)),
            "{cmd} with no arg must prompt for a task, not launch"
        );
    }
}

#[test]
fn slash_unknown_command_returns_hint_not_a_swallow() {
    let req = make_request("/frobnicate", RequestOrigin::Chat);
    match resolve(&req) {
        TriggerAction::UnknownSlashHint(name) => assert_eq!(name, "frobnicate"),
        _ => panic!("expected UnknownSlashHint"),
    }
}

#[test]
fn slash_registered_noncoding_command_forwards_as_normal_turn() {
    let req = make_request("/help", RequestOrigin::Chat);
    assert!(matches!(resolve(&req), TriggerAction::NormalTurn));
}

// -- resolve(): chat-intent detection -----------------------------------------------

#[test]
fn chat_intent_positive_on_chat_origin_returns_confirm_then_launch() {
    let req = make_request("implement this feature", RequestOrigin::Chat);
    match resolve(&req) {
        TriggerAction::ConfirmThenLaunch(kind, task) => {
            assert_eq!(kind, RunKind::Build);
            assert_eq!(task, "implement this feature");
        }
        _ => panic!("expected ConfirmThenLaunch"),
    }
}

#[test]
fn chat_intent_never_fires_on_cli_origin_even_with_coding_shaped_text() {
    let req = make_request("implement this feature", RequestOrigin::Cli);
    assert!(
        matches!(resolve(&req), TriggerAction::NormalTurn),
        "Cli origin (the eval bypass path) must never intent-launch"
    );
}

#[test]
fn ambiguous_chat_message_falls_through_to_a_normal_turn() {
    let req = make_request("hey, how's it going today?", RequestOrigin::Chat);
    assert!(matches!(resolve(&req), TriggerAction::NormalTurn));
}

#[test]
fn task_prompt_hint_names_the_matching_slash_command() {
    assert!(task_prompt_hint(RunKind::Plan).contains("/plan"));
    assert!(task_prompt_hint(RunKind::Build).contains("/code"));
}

// -- confirm-gate + launch integration ----------------------------------------------

/// Boots a real `AppHandle` (real DB/KMS/Orchestrator, no configured target repo) with one
/// `MockAdapter` registered — mirrors `crate::tests`'s own bootstrap convention. The `TempDir`
/// guard must be returned alongside the handle (not dropped inside this helper) or the DB file
/// disappears out from under the still-running app.
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

/// Drains `resp_rx` until `Complete`, invoking `on_chunk` for every chunk seen before it.
async fn drain_until_complete(
    resp_rx: &mut mpsc::Receiver<ResponseChunk>,
    mut on_chunk: impl FnMut(&ResponseChunk),
) {
    while let Some(chunk) = resp_rx.recv().await {
        let done = matches!(chunk, ResponseChunk::Complete);
        on_chunk(&chunk);
        if done {
            break;
        }
    }
}

/// HIGH: an approved confirm launches the pipeline, not a normal chat turn. This fixture's
/// `AppHandle` has no `coding.default_repo` preference set, so `launch()` deterministically
/// fails at repo resolution — that specific, LLM-independent error message is the proof the
/// launch path ran (a normal turn would instead surface the mock LLM's own text).
#[tokio::test]
async fn confirm_then_launch_approve_attempts_the_launch_not_a_normal_turn() {
    let (handle, _dir) = bootstrapped().await;
    let req = make_request("implement this feature", RequestOrigin::Chat);
    let session_id = req.session_id;

    let (resp_tx, mut resp_rx) = mpsc::channel(16);
    let handles = LaunchHandles {
        orc: Arc::clone(&handle.orchestrator),
        am: handle.adapters.clone(),
        tasks: handle.tasks.clone(),
    };
    let turn_cancel = handle.shutdown.child_token();

    tokio::spawn(confirm_then_launch(
        handles,
        turn_cancel,
        RunKind::Build,
        "implement this feature".to_string(),
        req,
        resp_tx,
    ));

    let approval_id = match tokio::time::timeout(std::time::Duration::from_secs(5), resp_rx.recv())
        .await
        .expect("confirm prompt must arrive")
        .expect("channel still open")
    {
        ResponseChunk::ToolApprovalRequest {
            approval_id, tool, ..
        } => {
            assert_eq!(tool, "run_build");
            approval_id
        }
        other => panic!("expected ToolApprovalRequest, got {other:?}"),
    };

    assert!(
        handle
            .orchestrator
            .approval_resolver()
            .resolve(approval_id, session_id, true),
        "resolve must find the pending confirm"
    );

    let mut saw_repo_error = false;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        drain_until_complete(&mut resp_rx, |chunk| {
            if let ResponseChunk::Error(msg) = chunk {
                saw_repo_error = msg.contains("no target repo resolved");
            }
        }),
    )
    .await
    .expect("launch attempt must terminate with Complete");

    assert!(
        saw_repo_error,
        "expected the launch's own repo-resolution failure, proving the launch path ran"
    );

    handle.shutdown(std::time::Duration::from_secs(5)).await;
}

/// HIGH: a denied confirm falls through to a normal chat turn — the mock LLM's completion text
/// appearing is the proof `run_normal_turn`/`orc.process` ran instead of `launch()`.
#[tokio::test]
async fn confirm_then_launch_deny_falls_through_to_a_normal_turn() {
    let (handle, _dir) = bootstrapped().await;
    let base_url = spawn_slow_llm_server(std::time::Duration::ZERO).await;
    handle.orchestrator.reload_llm(cloud_config(base_url)).await;

    let req = make_request("implement this feature", RequestOrigin::Chat);
    let session_id = req.session_id;

    let (resp_tx, mut resp_rx) = mpsc::channel(16);
    let handles = LaunchHandles {
        orc: Arc::clone(&handle.orchestrator),
        am: handle.adapters.clone(),
        tasks: handle.tasks.clone(),
    };
    let turn_cancel = handle.shutdown.child_token();

    tokio::spawn(confirm_then_launch(
        handles,
        turn_cancel,
        RunKind::Build,
        "implement this feature".to_string(),
        req,
        resp_tx,
    ));

    let approval_id = match tokio::time::timeout(std::time::Duration::from_secs(5), resp_rx.recv())
        .await
        .expect("confirm prompt must arrive")
        .expect("channel still open")
    {
        ResponseChunk::ToolApprovalRequest { approval_id, .. } => approval_id,
        other => panic!("expected ToolApprovalRequest, got {other:?}"),
    };

    assert!(handle
        .orchestrator
        .approval_resolver()
        .resolve(approval_id, session_id, false));

    let mut saw_mock_completion = false;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        drain_until_complete(&mut resp_rx, |chunk| {
            if let ResponseChunk::Text(t) = chunk {
                if t.contains("mock completion") {
                    saw_mock_completion = true;
                }
            }
        }),
    )
    .await
    .expect("normal turn must terminate with Complete");

    assert!(
        saw_mock_completion,
        "a denied confirm must fall through to a real chat turn"
    );

    handle.shutdown(std::time::Duration::from_secs(5)).await;
}
