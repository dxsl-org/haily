/// Delegate tool — routes a task to a domain-specific sub-agent (L1 or L2).
///
/// One `DelegateTool` instance exists per domain. The L0 LLM calls
/// `delegate_to_<domain>(task, context?)` when it decides the request
/// requires a domain specialist. The tool runs `run_sub_turn()` with the
/// domain's system prompt and tool whitelist, then returns the result as
/// a plain string that the L0 LLM incorporates into its final response.
use anyhow::Result;
use async_trait::async_trait;
use haily_db::DbHandle;
use haily_kms::KmsHandle;
use haily_llm::{LlmRouter, Tier};
use haily_tools::{RiskTier, Tool, ToolContext, ToolRegistry};
use haily_types::ResponseChunk;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Duration;

const SUB_TURN_TIMEOUT_SECS: u64 = 120;
/// Max characters for the task + context payload sent to a sub-agent.
/// Prevents context window overflows on weak local models.
const MAX_TASK_CHARS: usize = 4096;

/// Strip tool markup tags from user-supplied text to prevent prompt injection,
/// then clamp to `MAX_TASK_CHARS` on a char boundary. If a task string contains a
/// literal `<tool_call>` block, a careless LLM might echo it back as its first
/// response and trigger unintended tool calls.
fn sanitize_delegate_input(raw: &str) -> String {
    crate::tool_call::strip_tool_tags(raw)
        .chars()
        .take(MAX_TASK_CHARS)
        .collect()
}

pub struct DelegateTool {
    /// Tool name exposed to the LLM, e.g. "delegate_to_developer".
    pub tool_name: &'static str,
    /// Description injected into the L0 tool reference block.
    pub description: &'static str,
    /// System prompt for the sub-agent turn.
    pub system_prompt: &'static str,
    /// Human-readable domain label used in tracing.
    pub domain_name: &'static str,
    /// Shared handles needed to run the sub-turn.
    pub db: Arc<DbHandle>,
    pub kms: Arc<KmsHandle>,
    /// The SAME `Arc<RwLock<Arc<LlmRouter>>>` `Orchestrator` holds (F5 fix) — a
    /// frozen `Arc<LlmRouter>` captured at construction would never observe
    /// `reload_llm()`. Read-cloned under a brief lock per sub-turn in `execute`,
    /// mirroring `Orchestrator::process`'s rule: never hold the lock across await.
    pub llm: Arc<RwLock<Arc<LlmRouter>>>,
    /// Domain-filtered registry — only tools on the whitelist.
    pub sub_registry: Arc<ToolRegistry>,
    /// Maximum depth at which this tool will actually delegate.
    /// Calls from depth >= max_depth return a fallback string instead of spawning.
    pub max_depth: u8,
    /// Model tier this domain/specialist prefers (Phase 7 tier foundation). `None`
    /// for every config today — passed through to `run_sub_turn`'s completion calls.
    pub model_tier: Option<Tier>,
    /// Phase 3 kill switch (C8): the SAME `Arc<AtomicBool>` the Orchestrator holds,
    /// threaded into every `SubTurnRequest` so a sub-turn write observes
    /// `safety.disable_writes` too. Cloned into the request in `execute`.
    pub kill: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        self.tool_name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Clear description of what the domain agent should do. Include all relevant context from the conversation."
                },
                "context": {
                    "type": "string",
                    "description": "Optional: additional background or constraints for the agent."
                }
            },
            "required": ["task"]
        })
    }

    fn risk_tier(&self, _args: &serde_json::Value) -> RiskTier {
        RiskTier::ReversibleWrite
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        // Depth guard: prevents runaway nesting.
        if ctx.depth >= self.max_depth {
            tracing::warn!(
                domain = self.domain_name,
                depth = ctx.depth,
                "max_depth reached — handling inline"
            );
            // Return a neutral string so the parent LLM handles it gracefully.
            return Ok("Tôi sẽ xử lý trực tiếp.".into());
        }

        let raw_task = args["task"].as_str().unwrap_or("").trim().to_string();
        if raw_task.is_empty() {
            return Ok("Vui lòng mô tả rõ hơn yêu cầu.".into());
        }

        // Sanitize: strip tool markup to prevent injection, clamp length.
        let task = sanitize_delegate_input(&raw_task);
        let full_task = match args["context"].as_str().filter(|s| !s.is_empty()) {
            Some(ctx_text) => {
                let safe_ctx = sanitize_delegate_input(ctx_text);
                format!("{task}\n\n[Context: {safe_ctx}]")
            }
            None => task,
        };

        tracing::info!(
            domain = self.domain_name,
            depth = ctx.depth + 1,
            task_len = full_task.len(),
            "delegating to domain agent"
        );

        // Clone the Arc under a brief read-lock — never hold the lock across await
        // (same rule as `Orchestrator::process`). This is what makes `reload_llm()`
        // reach an in-flight or future delegation: every call reads the CURRENT
        // router instead of one frozen at `DelegateTool` construction time.
        let llm = Arc::clone(&*self.llm.read().unwrap_or_else(|e| e.into_inner()));

        // Phase 2 seam wiring:
        //  - `sub_tx`/`sub_rx`: the sub-turn's local response channel. The forwarder
        //    below drains `sub_rx` and relays ONLY `ToolApprovalRequest` upstream to
        //    the parent's real `ctx.approval_tx` — sub-agent `Text`/`ToolResult` are
        //    discarded (never surfaced as narration to the user).
        //  - `child`: a `child_token()` of the parent turn's cancel. Cancelling the
        //    parent cancels this sub-turn; a sub-turn timeout cancels ONLY this child,
        //    never the parent (the `child_token()` asymmetry).
        //  - `pause_tx`/`pause_rx`: the M3 clock-pause pulse. Each time the forwarder
        //    relays an approval request (a human-wait begins), it pulses; the timeout
        //    loop below re-arms a fresh `SUB_TURN_TIMEOUT_SECS` window on each pulse, so
        //    the wall-clock spent waiting for a human decision is excluded from the
        //    sub-turn compute budget.
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<ResponseChunk>(32);
        let child = ctx.cancel.child_token();
        let (pause_tx, mut pause_rx) = tokio::sync::mpsc::channel::<()>(8);

        let forwarder = tokio::spawn(approval_forwarder(
            sub_rx,
            ctx.approval_tx.clone(),
            pause_tx,
        ));

        // Phase 2: `full_task` and `domain_name` are exactly what `run_sub_turn` uses to
        // select `## Playbooks` (Jaccard over the task, filtered to this domain) and
        // `## Standards`. No new field is needed — the task string already flows here, and
        // `domain_name` is server-derived (never LLM-forged). Delegation invariants stay
        // intact: `run_sub_turn`'s `LoopGuard` still terminates on a tripped guard (never
        // feeds the error back), and every authored playbook/standard body is
        // tag-stripped before it enters the sub-agent prompt.
        let sub_turn = crate::agent::run_sub_turn(crate::agent::SubTurnRequest {
            task: full_task,
            system_prompt: self.system_prompt,
            domain_name: self.domain_name,
            depth: ctx.depth + 1,
            db: Arc::clone(&self.db),
            kms: Arc::clone(&self.kms),
            llm,
            tools: Arc::clone(&self.sub_registry),
            session_id: ctx.session_id,
            model_tier: self.model_tier,
            approval_gate: Arc::clone(&ctx.approval_gate),
            cancel: child.clone(),
            approval_tx: sub_tx,
            kill: Arc::clone(&self.kill),
            // Harness Completion phase 2: reuse the CALLING context's turn identity/counter
            // rather than minting a fresh one. A delegated sub-turn is part of the turn that
            // requested it (the parent LLM chose to delegate mid-turn), not a new logical
            // unit of work — its journal rows must undo together with the parent's under one
            // `undo_turn(turn_id, session_id)` call, and its re-tiered deletes must count
            // toward the SAME M2 cap so delegation cannot be used to bypass the ceiling.
            // `ctx` here is whatever turn/sub-turn CALLED this tool — at L0 that is the root
            // turn's id/counter; for a NESTED delegation (a sub-agent that itself delegates)
            // it is that sub-turn's `ToolContext`, which already carries the root turn's
            // values forward (this same rule applied one level up) — so the whole delegation
            // chain converges on one shared turn_id/counter, however deep it nests.
            turn_id: ctx.turn_id,
            turn_deletes: Arc::clone(&ctx.turn_deletes),
            // A delegated sub-turn is a chat-scale unit, not a pipeline stage: keep the
            // global LoopGuard ceiling and no run grouping. Only the P4b pipeline runner,
            // which calls `run_sub_turn` directly (never via this tool), sets these.
            max_tool_calls: None,
            run_id: None,
            // A delegated sub-turn never forces a generation grammar — only a pipeline stage
            // (the runner) does. Keep it unconstrained here (P5 additive default).
            grammar: None,
        });

        let result = run_with_pausable_timeout(
            Duration::from_secs(SUB_TURN_TIMEOUT_SECS),
            sub_turn,
            &mut pause_rx,
        )
        .await;

        let outcome = match result {
            Some(Ok(response)) => Ok(response),
            Some(Err(e)) => {
                tracing::warn!(domain = self.domain_name, error = %e, "sub-agent failed");
                Ok(format!("Không thể hoàn thành yêu cầu lúc này: {e:#}"))
            }
            None => {
                // Elapse: cancel ONLY the child token (never the parent turn), which
                // unblocks any sub-turn await and closes `sub_tx`, letting the forwarder
                // drain to completion below.
                child.cancel();
                tracing::warn!(
                    domain = self.domain_name,
                    timeout_secs = SUB_TURN_TIMEOUT_SECS,
                    "sub-agent timed out"
                );
                Ok("Agent mất quá nhiều thời gian — tôi sẽ cố gắng trả lời trực tiếp.".into())
            }
        };

        // INVARIANT (mirrors dispatch.rs's delivery-forwarder rule): the forwarder MUST
        // be joined on EVERY exit path — success, sub-turn error, and timeout-elapse —
        // or a leaked task could relay a stale approval into a later turn. `sub_tx` is
        // dropped when `run_sub_turn` returns (or `child.cancel()` above unblocks it),
        // so `sub_rx.recv()` yields `None` and the forwarder terminates; this await
        // just confirms it has.
        let _ = forwarder.await;
        outcome
    }
}

/// Drain a sub-turn's response channel, relaying ONLY `ToolApprovalRequest` chunks
/// upstream to the parent's real `parent_tx` — sub-agent `Text`/`ToolResult`/`Complete`
/// narration is discarded (never surfaced to the user). Each relayed approval pulses
/// `pause_tx` to signal the M3 clock-pause (a human-wait is starting).
///
/// Terminates when `sub_rx` closes (the sub-turn dropped its `sub_tx`) or the parent
/// stream is gone. Must be joined by the caller on every exit path so a leaked task
/// cannot relay a stale approval into a later turn.
///
/// `pub(crate)` so the P4b pipeline runner reuses the SAME forwarder rather than duplicating
/// it — a stage's IrreversibleWrite must reach the real user through this exact relay (SEC-H).
pub(crate) async fn approval_forwarder(
    mut sub_rx: tokio::sync::mpsc::Receiver<ResponseChunk>,
    parent_tx: tokio::sync::mpsc::Sender<ResponseChunk>,
    pause_tx: tokio::sync::mpsc::Sender<()>,
) {
    while let Some(chunk) = sub_rx.recv().await {
        if matches!(chunk, ResponseChunk::ToolApprovalRequest { .. }) {
            // Best-effort pulse: a full channel just means one is already queued.
            let _ = pause_tx.try_send(());
            // Relay to the real user. If the parent stream is gone, stop relaying but
            // keep draining so the sub-turn is never wedged on a full channel.
            if parent_tx.send(chunk).await.is_err() {
                break;
            }
        }
        // Non-approval chunks are discarded.
    }
}

/// Run `fut` under a wall-clock `timeout` that PAUSES across a human-wait (M3).
///
/// Instead of a flat `tokio::time::timeout`, this races `fut` against a sleep that
/// RE-ARMS to a fresh `timeout` window every time a pulse arrives on `pause_rx`. The
/// forwarder pulses `pause_rx` whenever it relays a nested `ToolApprovalRequest`, so
/// the time a sub-turn spends blocked in `broker.request` (waiting for the user) is
/// excluded from its compute budget: a 90s nested approval on a 60s-elapsed sub-turn
/// does NOT trip a 120s limit.
///
/// Returns `Some(fut_output)` on completion, or `None` on a genuine compute timeout
/// (the deadline elapsed with no pending approval). `biased` select prefers the
/// future's completion and the pause pulse over the sleep, so a just-resolved approval
/// cannot lose a race to a simultaneously-firing stale deadline.
///
/// `pub(crate)` so the P4b pipeline runner reuses the SAME pausable-clock helper — a stage
/// blocked on a nested approval must have its compute budget paused exactly as a delegation does.
pub(crate) async fn run_with_pausable_timeout<F>(
    timeout: Duration,
    fut: F,
    pause_rx: &mut tokio::sync::mpsc::Receiver<()>,
) -> Option<F::Output>
where
    F: std::future::Future,
{
    tokio::pin!(fut);
    loop {
        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            res = &mut fut => break Some(res),
            Some(()) = pause_rx.recv() => continue, // human-wait began — arm a fresh window
            _ = &mut sleep => break None,            // genuine compute timeout
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_all_tool_markup() {
        let raw =
            "làm việc <tool_call>{\"tool\":\"x\"}</tool_call> và <tool_result>y</tool_result>";
        let out = sanitize_delegate_input(raw);
        assert!(!out.contains("<tool_call>"));
        assert!(!out.contains("</tool_call>"));
        assert!(!out.contains("<tool_result>"));
        assert!(!out.contains("</tool_result>"));
        // Non-markup content is preserved.
        assert!(out.contains("làm việc"));
    }

    #[test]
    fn sanitize_clamps_to_max_chars() {
        let raw = "a".repeat(MAX_TASK_CHARS + 500);
        let out = sanitize_delegate_input(&raw);
        assert_eq!(out.chars().count(), MAX_TASK_CHARS);
    }

    #[test]
    fn sanitize_clamp_respects_char_boundaries() {
        // Multibyte chars must not be split mid-codepoint when clamping.
        let raw = "é".repeat(MAX_TASK_CHARS + 100);
        let out = sanitize_delegate_input(&raw);
        assert_eq!(out.chars().count(), MAX_TASK_CHARS);
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn sanitize_leaves_clean_input_untouched() {
        let raw = "Nghiên cứu ETF index fund";
        assert_eq!(sanitize_delegate_input(raw), raw);
    }
}

#[cfg(test)]
mod reload_propagation_tests {
    //! Phase 7 (F5): proves `DelegateTool` reads the router through the SAME
    //! `Arc<RwLock<Arc<LlmRouter>>>` `Orchestrator::reload_llm` writes to, instead of
    //! a frozen `Arc<LlmRouter>` captured at construction. The mock server below
    //! echoes back its own `model` field as the completion text — the only reliable
    //! way to prove a delegated sub-turn actually reached the NEW backend, since
    //! `provider_name()` returns the same `"cloud"` label for both.
    use super::*;
    use haily_db::DbHandle;
    use haily_kms::KmsHandle;
    use haily_llm::LlmConfig;
    use haily_tools::ToolRegistry;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use uuid::Uuid;

    async fn spawn_model_echo_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request_text = String::from_utf8_lossy(&buf[..n]);
                    let body_start = request_text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                    let model =
                        serde_json::from_str::<serde_json::Value>(&request_text[body_start..])
                            .ok()
                            .and_then(|v| v["model"].as_str().map(str::to_string))
                            .unwrap_or_else(|| "unknown".to_string());

                    let payload = serde_json::json!({
                        "choices": [{ "message": { "content": model } }]
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        format!("http://{addr}")
    }

    fn cloud_config(base_url: String, model: &str) -> LlmConfig {
        LlmConfig {
            cloud_api_keys: vec!["test-key".to_string()],
            cloud_base_url: base_url,
            cloud_model: model.to_string(),
            ..LlmConfig::default()
        }
    }

    #[tokio::test]
    async fn reload_llm_reaches_the_next_delegated_sub_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("haily.db");
        let db = Arc::new(DbHandle::init(&db_path).await.expect("db init"));
        let kms = Arc::new(
            KmsHandle::init((*db).clone(), dir.path())
                .await
                .expect("kms init"),
        );

        let base_url = spawn_model_echo_server().await;
        let llm = Arc::new(RwLock::new(Arc::new(
            LlmRouter::init(cloud_config(base_url.clone(), "model-before-reload")).await,
        )));

        let delegate = DelegateTool {
            tool_name: "delegate_to_test",
            description: "test",
            system_prompt: "test system prompt",
            domain_name: "test",
            db: Arc::clone(&db),
            kms: Arc::clone(&kms),
            llm: Arc::clone(&llm),
            sub_registry: Arc::new(ToolRegistry::new()),
            max_depth: 1,
            model_tier: None,
            kill: Arc::new(AtomicBool::new(false)),
        };

        let (approval_tx, _rx) = tokio::sync::mpsc::channel(8);
        let ctx = ToolContext {
            db: Arc::clone(&db),
            kms: Arc::clone(&kms),
            session_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            depth: 0,
            domain: None,
            approval_gate: Arc::new(crate::approval::ApprovalBroker::new()),
            approval_tx,
            cancel: tokio_util::sync::CancellationToken::new(),
            turn_deletes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_journal_id: Arc::new(std::sync::Mutex::new(None)),
            run_id: None,
        };
        let args = serde_json::json!({ "task": "short task before reload" });
        let before = delegate
            .execute(args, &ctx)
            .await
            .expect("execute before reload");
        assert!(
            before.contains("model-before-reload"),
            "expected pre-reload model in response, got: {before}"
        );

        // Simulate `Orchestrator::reload_llm` — swap the Arc under the SAME lock the
        // DelegateTool holds, exactly as `reload_llm` does on `Orchestrator::llm`.
        let new_router =
            Arc::new(LlmRouter::init(cloud_config(base_url, "model-after-reload")).await);
        {
            let mut guard = llm.write().unwrap_or_else(|e| e.into_inner());
            *guard = new_router;
        }

        let args = serde_json::json!({ "task": "short task after reload" });
        let after = delegate
            .execute(args, &ctx)
            .await
            .expect("execute after reload");
        assert!(
            after.contains("model-after-reload"),
            "expected the sub-turn to use the RELOADED model, got: {after} — DelegateTool is still reading a frozen router"
        );
    }
}

#[cfg(test)]
mod seam_tests {
    //! Phase 2 — the sub-agent approval seam: the forwarder (relay-only + join), the
    //! `child_token()` asymmetry, and the M3 pausable-timeout clock. These exercise
    //! the SAME `approval_forwarder`/`run_with_pausable_timeout` functions
    //! `DelegateTool::execute` calls (not copies), so a regression there fails here.
    use super::*;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    /// The forwarder relays ONLY `ToolApprovalRequest` upstream; sub-agent
    /// `Text`/`ToolResult`/`Complete` narration is discarded and never reaches the
    /// parent tx (guards the narration-leak hazard).
    #[tokio::test]
    async fn forwarder_relays_only_approval_requests() {
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<ResponseChunk>(32);
        let (parent_tx, mut parent_rx) = tokio::sync::mpsc::channel::<ResponseChunk>(32);
        let (pause_tx, _pause_rx) = tokio::sync::mpsc::channel::<()>(8);

        let forwarder = tokio::spawn(approval_forwarder(sub_rx, parent_tx, pause_tx));

        // Interleave sub-agent narration with a single real approval request.
        sub_tx
            .send(ResponseChunk::Text("thinking...".into()))
            .await
            .unwrap();
        sub_tx
            .send(ResponseChunk::ToolResult {
                name: "note_save".into(),
                ok: true,
                reversible: false,
                journal_id: None,
            })
            .await
            .unwrap();
        let approval_id = Uuid::new_v4();
        sub_tx
            .send(ResponseChunk::ToolApprovalRequest {
                tool: "delete_thing".into(),
                args: "{}".into(),
                approval_id,
                origin: Some("L1:developer".into()),
                reversible: false,
            })
            .await
            .unwrap();
        sub_tx.send(ResponseChunk::Complete).await.unwrap();
        drop(sub_tx); // close the channel so the forwarder terminates

        forwarder.await.unwrap();

        // Exactly ONE chunk reached the parent — the approval request — and nothing else.
        let relayed = parent_rx
            .recv()
            .await
            .expect("the approval request must be relayed");
        match relayed {
            ResponseChunk::ToolApprovalRequest {
                approval_id: got,
                origin,
                ..
            } => {
                assert_eq!(got, approval_id);
                assert_eq!(origin.as_deref(), Some("L1:developer"));
            }
            other => panic!("expected the approval request, got {other:?}"),
        }
        assert!(
            parent_rx.recv().await.is_none(),
            "no sub-agent Text/ToolResult/Complete narration may leak to the parent"
        );
    }

    /// The forwarder task terminates (is joinable) once the sub-turn drops its `sub_tx`
    /// — i.e. it does not leak on the timeout path where `child.cancel()` unblocks the
    /// sub-turn and drops the sender (guards the round-2 forwarder-race hazard).
    #[tokio::test]
    async fn forwarder_joined_on_timeout_path() {
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<ResponseChunk>(32);
        let (parent_tx, _parent_rx) = tokio::sync::mpsc::channel::<ResponseChunk>(32);
        let (pause_tx, _pause_rx) = tokio::sync::mpsc::channel::<()>(8);

        let forwarder = tokio::spawn(approval_forwarder(sub_rx, parent_tx, pause_tx));

        // Simulate the timeout-elapse path: the sub-turn is dropped (its `sub_tx` goes
        // away) without the channel ever being explicitly closed by a graceful return.
        drop(sub_tx);

        // The forwarder must terminate promptly, proving the join in `execute` cannot
        // hang and the task is not leaked.
        tokio::time::timeout(Duration::from_secs(2), forwarder)
            .await
            .expect("forwarder must terminate when sub_tx is dropped, not leak")
            .expect("forwarder task panicked");
    }

    /// A child-token cancellation (a sub-turn timeout) cancels ONLY the child; the
    /// parent (L0) token stays live — a sub-turn timeout must never abort the whole
    /// turn (the `child_token()` asymmetry).
    #[tokio::test]
    async fn child_token_timeout_does_not_cancel_parent() {
        let parent = CancellationToken::new();
        let l1 = parent.child_token();
        let l2 = l1.child_token();

        // An L2 timeout cancels only the L2 child.
        l2.cancel();
        assert!(
            l2.is_cancelled(),
            "the child being timed out must be cancelled"
        );
        assert!(!l1.is_cancelled(), "the intermediate parent must stay live");
        assert!(
            !parent.is_cancelled(),
            "the L0 parent token must stay live after a sub-turn timeout"
        );

        // Conversely, cancelling the parent DOES propagate down (shutdown drains all).
        parent.cancel();
        assert!(
            l1.is_cancelled(),
            "parent cancel must propagate to the child"
        );
    }

    /// M3: the sub-turn clock is PAUSED across a human-wait. With a short 100ms budget
    /// and a "compute" future that would take ~400ms, a flat timeout would fire; but
    /// pausing (a pulse every 30ms while the wait is in progress) re-arms the window so
    /// the future completes. Tests the APPROVAL_TIMEOUT < T < SUB_TURN window shape at
    /// small scale (real 120s coincidence is untestable in a unit test).
    #[tokio::test]
    async fn sub_turn_clock_paused_across_broker_wait() {
        let (pause_tx, mut pause_rx) = tokio::sync::mpsc::channel::<()>(8);

        // A future modelling a sub-turn that is BLOCKED in a nested approval for 400ms
        // (four 100ms budgets long) while the human decides; it pulses `pause_tx` every
        // 30ms to signal the ongoing human-wait, then completes.
        let pulser = pause_tx.clone();
        let work = async move {
            for _ in 0..13 {
                tokio::time::sleep(Duration::from_millis(30)).await;
                let _ = pulser.try_send(());
            }
            "done"
        };

        // Budget shorter than the work — WITHOUT the pause this would elapse to None.
        let out = run_with_pausable_timeout(Duration::from_millis(100), work, &mut pause_rx).await;
        assert_eq!(
            out,
            Some("done"),
            "a paused (approval-pending) sub-turn must NOT trip its compute timeout"
        );
    }

    /// Control for the M3 test: with NO pulses, the same short budget DOES elapse to a
    /// genuine timeout — proving the pause is what saved the test above, not a loose
    /// budget.
    #[tokio::test]
    async fn unpaused_clock_still_times_out() {
        let (_pause_tx, mut pause_rx) = tokio::sync::mpsc::channel::<()>(8);
        let work = async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            "done"
        };
        let out = run_with_pausable_timeout(Duration::from_millis(100), work, &mut pause_rx).await;
        assert_eq!(
            out, None,
            "a compute-bound sub-turn with no human-wait must still time out"
        );
    }
}
