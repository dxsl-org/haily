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
use haily_tools::{Tool, ToolClass, ToolContext, ToolRegistry};
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

    fn approval_class(&self) -> ToolClass {
        ToolClass::AutoApprove
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

        let result = tokio::time::timeout(
            Duration::from_secs(SUB_TURN_TIMEOUT_SECS),
            crate::agent::run_sub_turn(crate::agent::SubTurnRequest {
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
            }),
        )
        .await;

        match result {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(e)) => {
                tracing::warn!(domain = self.domain_name, error = %e, "sub-agent failed");
                Ok(format!("Không thể hoàn thành yêu cầu lúc này: {e:#}"))
            }
            Err(_elapsed) => {
                tracing::warn!(domain = self.domain_name, timeout_secs = SUB_TURN_TIMEOUT_SECS, "sub-agent timed out");
                Ok("Agent mất quá nhiều thời gian — tôi sẽ cố gắng trả lời trực tiếp.".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_all_tool_markup() {
        let raw = "làm việc <tool_call>{\"tool\":\"x\"}</tool_call> và <tool_result>y</tool_result>";
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
                let Ok((mut stream, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request_text = String::from_utf8_lossy(&buf[..n]);
                    let body_start = request_text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                    let model = serde_json::from_str::<serde_json::Value>(&request_text[body_start..])
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
        let kms = Arc::new(KmsHandle::init((*db).clone()).await.expect("kms init"));

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
        };

        let ctx = ToolContext { db: Arc::clone(&db), kms: Arc::clone(&kms), session_id: Uuid::new_v4(), depth: 0 };
        let args = serde_json::json!({ "task": "short task before reload" });
        let before = delegate.execute(args, &ctx).await.expect("execute before reload");
        assert!(before.contains("model-before-reload"), "expected pre-reload model in response, got: {before}");

        // Simulate `Orchestrator::reload_llm` — swap the Arc under the SAME lock the
        // DelegateTool holds, exactly as `reload_llm` does on `Orchestrator::llm`.
        let new_router = Arc::new(LlmRouter::init(cloud_config(base_url, "model-after-reload")).await);
        {
            let mut guard = llm.write().unwrap_or_else(|e| e.into_inner());
            *guard = new_router;
        }

        let args = serde_json::json!({ "task": "short task after reload" });
        let after = delegate.execute(args, &ctx).await.expect("execute after reload");
        assert!(
            after.contains("model-after-reload"),
            "expected the sub-turn to use the RELOADED model, got: {after} — DelegateTool is still reading a frozen router"
        );
    }
}
