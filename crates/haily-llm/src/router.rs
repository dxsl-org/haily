use crate::CloudClient;
#[cfg(feature = "llama")]
use crate::prompt::PromptFormat;
use crate::{CompletionRequest, LlmClient, StreamChunk};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Placeholder used when no backend is configured.
/// Returns a user-visible error on every completion so the app still starts.
struct NoopClient;

#[async_trait]
impl LlmClient for NoopClient {
    async fn complete(&self, _req: CompletionRequest) -> Result<String> {
        Err(anyhow::anyhow!(
            "Chưa cấu hình LLM. Mở Settings → Model LLM để chọn model."
        ))
    }

    async fn complete_stream(&self, _req: CompletionRequest) -> Result<mpsc::Receiver<StreamChunk>> {
        Err(anyhow::anyhow!(
            "Chưa cấu hình LLM. Mở Settings → Model LLM để chọn model."
        ))
    }

    fn provider_name(&self) -> &str { "unconfigured" }
    fn context_window(&self) -> u32 {
        // Never actually budgeted against — every `complete()` call errors first.
        // A conservative non-zero value avoids a divide-by-zero-shaped footgun if a
        // caller ever budgets before checking `provider_name()`.
        DEFAULT_LLAMA_N_CTX
    }
}

/// Per-provider context-window constants used for budgeting (`context_window()`).
/// Clamped, not the provider's true native maximum, so history sizing stays sane
/// immediately after a hot-swap between backends with very different windows.
pub(crate) const DEFAULT_LLAMA_N_CTX: u32 = 8192;
/// Cloud providers advertise context windows far larger than a local model
/// (e.g. 128k-200k) — clamped to 32k for budgeting per phase-05 spec so a session's
/// history doesn't balloon to an unreasonable size just because the backend changed.
pub(crate) const CLOUD_CONTEXT_WINDOW_CLAMP: u32 = 32_000;

#[cfg(feature = "llama")]
use crate::LlamaClient;

/// Model-tier foundation (Phase 7 — wired but inert): names a routing tier a
/// `DomainConfig`/`SpecialistConfig` can request. Full complexity-based
/// auto-routing is deliberately NOT implemented (YAGNI until a task-outcome
/// quality signal exists) — today a tier only changes which *cloud model name*
/// a completion uses, never the backend/provider itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    Fast,
    Medium,
    Thinking,
}

/// Optional cloud model-name override per tier. Every field defaults to `None`,
/// meaning "no override — use `LlmConfig::cloud_model`" — this is the zero-behavior-
/// change default the phase-07 spec requires.
#[derive(Debug, Clone, Default)]
pub struct TierModels {
    pub fast: Option<String>,
    pub medium: Option<String>,
    pub thinking: Option<String>,
}

impl TierModels {
    fn get(&self, tier: Tier) -> Option<&str> {
        match tier {
            Tier::Fast => self.fast.as_deref(),
            Tier::Medium => self.medium.as_deref(),
            Tier::Thinking => self.thinking.as_deref(),
        }
    }
}

/// Runtime configuration for LLM routing.
/// Loaded from KMS preferences on startup; user can update without restart.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// One or more API keys for the cloud backend.
    /// Multiple keys rotate round-robin; empty = no cloud backend.
    pub cloud_api_keys: Vec<String>,
    pub cloud_base_url: String,
    pub cloud_model: String,
    /// Per-tier cloud model-name overrides — see `Tier`/`TierModels`. All `None`
    /// by default (no behavior change until a caller opts a domain/specialist in).
    pub tier_models: TierModels,

    /// Path to GGUF model file for embedded inference (only used with `llama` feature).
    #[cfg(feature = "llama")]
    pub llama_model_path: Option<std::path::PathBuf>,
    /// Context window size for llama.cpp (default 8192 — ~295MB KV cache for
    /// Qwen2.5-3B, acceptable on any laptop capable of running a 3B model; see
    /// research report 03 §A1). User-configurable via the `llm.llama_n_ctx` preference.
    #[cfg(feature = "llama")]
    pub llama_n_ctx: u32,
    /// Prompt format used when formatting messages for the GGUF model.
    /// ChatML for Qwen2.5; Gemma4 for google/gemma-4 GGUF files.
    #[cfg(feature = "llama")]
    pub llama_prompt_format: PromptFormat,
    /// Number of model layers to offload to GPU.
    /// 0 = CPU-only; 999 = full GPU offload (llama.cpp clamps to actual layer count).
    /// Auto-detected from compiled GPU features; override via `llm.llama_n_gpu_layers` preference.
    #[cfg(feature = "llama")]
    pub llama_n_gpu_layers: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        // Collect env-var keys available at startup (may be overridden by DB prefs).
        let env_keys: Vec<String> = ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "HAILY_CLOUD_KEY"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .collect();
        Self {
            cloud_api_keys: env_keys,
            cloud_base_url: "https://api.openai.com".into(),
            cloud_model: "gpt-4o-mini".into(),
            tier_models: TierModels::default(),
            #[cfg(feature = "llama")]
            llama_model_path: None,
            #[cfg(feature = "llama")]
            llama_n_ctx: DEFAULT_LLAMA_N_CTX,
            #[cfg(feature = "llama")]
            llama_prompt_format: PromptFormat::ChatML,
            #[cfg(feature = "llama")]
            llama_n_gpu_layers: crate::gpu::default_gpu_layers(),
        }
    }
}

/// Builds one `CloudClient` per tier that names a model override in
/// `config.tier_models`, reusing `config`'s base URL and API keys. Silently skips
/// (with a warning) a tier whose override is set but no cloud keys are configured —
/// `complete_tiered` degrades to `primary` in that case, which is always correct.
fn build_tier_clients(config: &LlmConfig) -> HashMap<Tier, Arc<dyn LlmClient>> {
    let mut clients: HashMap<Tier, Arc<dyn LlmClient>> = HashMap::new();
    if config.cloud_api_keys.is_empty() {
        return clients;
    }
    for (tier, model) in [
        (Tier::Fast, config.tier_models.get(Tier::Fast)),
        (Tier::Medium, config.tier_models.get(Tier::Medium)),
        (Tier::Thinking, config.tier_models.get(Tier::Thinking)),
    ] {
        let Some(model) = model else { continue };
        match CloudClient::new(config.cloud_base_url.clone(), config.cloud_api_keys.clone(), model) {
            Ok(client) => {
                clients.insert(tier, Arc::new(client));
            }
            Err(e) => tracing::warn!(?tier, "failed to build tier cloud client: {e:#}"),
        }
    }
    clients
}

/// Routes requests to the best available LLM backend.
///
/// Priority:
///   1. llama.cpp embedded  (feature = "llama", model file present)
///   2. Cloud API           (api_key configured)
pub struct LlmRouter {
    primary: Arc<dyn LlmClient>,
    fallback: Option<Arc<dyn LlmClient>>,
    /// One cloud client per tier that has a configured model-name override.
    /// An absent entry means "no override" — `complete_tiered` falls back to
    /// `primary` in that case. Built once at `init`/`reload_llm` time so a
    /// hot-swap picks up new tier overrides exactly like the primary does.
    tier_clients: HashMap<Tier, Arc<dyn LlmClient>>,
}

impl LlmRouter {
    /// Always succeeds — uses `NoopClient` when no backend is reachable so the
    /// app can start without a configured model. The error surfaces only when the
    /// user actually sends a message.
    pub async fn init(config: LlmConfig) -> Self {
        let cloud: Option<Arc<dyn LlmClient>> = if !config.cloud_api_keys.is_empty() {
            match CloudClient::new(
                config.cloud_base_url.clone(),
                config.cloud_api_keys.clone(),
                config.cloud_model.clone(),
            ) {
                Ok(client) => Some(Arc::new(client) as Arc<dyn LlmClient>),
                Err(e) => {
                    tracing::warn!("failed to build cloud HTTP client: {e:#}");
                    None
                }
            }
        } else {
            None
        };

        // Tier clients only make sense against the cloud backend (llama.cpp has a
        // single loaded GGUF file — there is no "different model" to route a tier
        // to locally). Built from the same base_url/keys as `cloud`, one extra
        // `CloudClient` per tier that names an override.
        let tier_clients = build_tier_clients(&config);

        #[cfg(feature = "llama")]
        {
            // `filter(|p| p.exists())` keeps the path and existence check tied together —
            // no separate boolean flag that could drift from the `Option` it was derived
            // from, and no `.unwrap()` needed to re-extract the path afterward.
            let existing_model_path = config
                .llama_model_path
                .clone()
                .filter(|p| p.exists());

            if let Some(path) = existing_model_path {
                let fmt = config.llama_prompt_format;
                let n_gpu_layers = config.llama_n_gpu_layers;
                tracing::info!(
                    "LLM: llama.cpp ({}) — {}",
                    path.display(),
                    crate::gpu::gpu_mode_label(n_gpu_layers)
                );
                match tokio::task::spawn_blocking(move || {
                    LlamaClient::load(path, config.llama_n_ctx, fmt, n_gpu_layers)
                }).await {
                    Ok(Ok(client)) => return Self {
                        primary: Arc::new(client),
                        fallback: cloud,
                        tier_clients,
                    },
                    Ok(Err(e)) => tracing::warn!("llama.cpp load failed: {e:#}"),
                    Err(e)     => tracing::warn!("llama.cpp spawn failed: {e:#}"),
                }
            }
        }

        if let Some(cloud_client) = cloud {
            tracing::info!("LLM: cloud API ({})", config.cloud_model);
            return Self { primary: cloud_client, fallback: None, tier_clients };
        }

        tracing::warn!("No LLM backend configured — open Settings → Model LLM");
        Self { primary: Arc::new(NoopClient), fallback: None, tier_clients }
    }

    /// Complete a request against a specific model `tier`. Falls back to `primary`
    /// (the default model) when `tier` is `None`, or when `Some(tier)` names a tier
    /// with no configured override — this is the "wired but inert" contract: callers
    /// may pass a tier freely and always get a working completion, with zero
    /// behavior change until an operator actually configures `LlmConfig::tier_models`.
    pub async fn complete_tiered(&self, tier: Option<Tier>, req: CompletionRequest) -> Result<String> {
        match tier.and_then(|t| self.tier_clients.get(&t)) {
            Some(client) => client.complete(req).await,
            None => self.complete(req).await,
        }
    }

    pub fn provider_name(&self) -> &str {
        self.primary.provider_name()
    }

    /// Context window (tokens) of the currently-active backend, for `haily-core`'s
    /// token budgeter. Reflects `primary`, not `fallback` — a request that spills
    /// over to the cloud fallback mid-flight is rare enough (and the fallback's
    /// window is typically larger, not smaller) that budgeting for the primary is
    /// the correct common case; see phase-05 spec.
    pub fn context_window(&self) -> u32 {
        self.primary.context_window()
    }

    /// Tries the cloud fallback (if configured) after a pre-first-token primary
    /// failure; returns `primary_err` unchanged if there is no fallback configured.
    /// Never called once the primary has emitted a token — see `complete_stream`'s
    /// doc comment for the fallback-scope rule this enforces.
    async fn fallback_stream_or_err(
        &self,
        req: CompletionRequest,
        primary_err: anyhow::Error,
    ) -> Result<mpsc::Receiver<StreamChunk>> {
        match &self.fallback {
            Some(fallback) => {
                tracing::warn!(
                    "trying cloud fallback stream after primary ({}) pre-first-token failure",
                    self.primary.provider_name()
                );
                fallback.complete_stream(req).await
            }
            None => Err(primary_err),
        }
    }
}

/// Bound on the router's own passthrough channel — mirrors the bound each backend
/// uses internally (llama: 64, cloud: 32); the router just relays, so any bound in
/// that ballpark is fine (KISS — no need to match exactly).
const ROUTER_STREAM_BOUND: usize = 64;

/// Re-emits `first` (already pulled off `src` to inspect it) followed by the rest of
/// `src`, on a fresh bounded channel. Runs as a spawned forwarding task so the
/// caller can return the receiver immediately without holding `src` open in the
/// trait method's own stack frame.
fn relay_with_first_item(
    backend_name: String,
    mut src: mpsc::Receiver<StreamChunk>,
    first: StreamChunk,
) -> mpsc::Receiver<StreamChunk> {
    let (tx, rx) = mpsc::channel(ROUTER_STREAM_BOUND);
    tokio::spawn(async move {
        if tx.send(first).await.is_err() {
            return; // consumer dropped — nothing left to relay
        }
        while let Some(chunk) = src.recv().await {
            if tx.send(chunk).await.is_err() {
                break;
            }
        }
        tracing::debug!(backend = %backend_name, "primary LLM stream relay finished");
    });
    rx
}

#[async_trait]
impl LlmClient for LlmRouter {
    async fn complete(&self, req: CompletionRequest) -> Result<String> {
        match self.primary.complete(req.clone()).await {
            Ok(text) => Ok(text),
            Err(primary_err) => {
                if let Some(fallback) = &self.fallback {
                    tracing::warn!(
                        "primary LLM ({}) failed: {primary_err:#}; trying cloud fallback",
                        self.primary.provider_name()
                    );
                    fallback.complete(req).await
                } else {
                    Err(primary_err)
                }
            }
        }
    }

    /// FALLBACK SCOPE (red-team constraint, phase-06): falls back to cloud ONLY when
    /// the primary backend fails before emitting a single token — either by
    /// returning `Err` from `complete_stream` itself, or by the FIRST item off its
    /// channel being `StreamChunk::Error`. Once any `Token` has been forwarded, a
    /// later primary-stream error is relayed as-is and the fallback is never
    /// consulted — re-running the whole request through a second backend at that
    /// point would re-stream the answer and duplicate user-visible text.
    async fn complete_stream(&self, req: CompletionRequest) -> Result<mpsc::Receiver<StreamChunk>> {
        let primary_name = self.primary.provider_name().to_string();
        let mut primary_rx = match self.primary.complete_stream(req.clone()).await {
            Ok(rx) => rx,
            Err(primary_err) => {
                tracing::warn!("primary LLM ({primary_name}) stream init failed: {primary_err:#}");
                return self.fallback_stream_or_err(req, primary_err).await;
            }
        };

        // Peek the first item to decide pre-first-token vs. post-first-token failure.
        match primary_rx.recv().await {
            None => {
                // Channel closed with no message at all — treat as a pre-first-token
                // init failure (same bucket as an immediate `Err`).
                let err = anyhow::anyhow!("{primary_name} stream closed before producing any output");
                tracing::warn!("primary LLM ({primary_name}) stream: {err:#}");
                self.fallback_stream_or_err(req, err).await
            }
            Some(StreamChunk::Error(msg)) => {
                let err = anyhow::anyhow!("{msg}");
                tracing::warn!("primary LLM ({primary_name}) stream failed before first token: {msg}");
                self.fallback_stream_or_err(req, err).await
            }
            Some(first @ StreamChunk::Token(_)) => {
                tracing::info!(backend = %primary_name, "streaming from primary LLM");
                Ok(relay_with_first_item(primary_name, primary_rx, first))
            }
            Some(first @ StreamChunk::Done { .. }) => {
                // Zero-token completion (e.g. empty response) — still a legitimate
                // primary stream, not a failure; forward as-is, no fallback.
                tracing::info!(backend = %primary_name, "streaming from primary LLM (empty output)");
                Ok(relay_with_first_item(primary_name, primary_rx, first))
            }
        }
    }

    fn provider_name(&self) -> &str {
        self.primary.provider_name()
    }

    fn context_window(&self) -> u32 {
        self.primary.context_window()
    }
}

#[cfg(test)]
mod tier_tests {
    //! `complete_tiered` fallback semantics (phase-07 tier foundation). The mock
    //! server below echoes back the `model` field it received in the request body
    //! as the completion text — the only reliable way to prove which cloud model
    //! name a given call actually used (both tiers speak through the same
    //! `CloudClient` HTTP shape, so `provider_name()` alone can't distinguish them).
    use super::*;
    use crate::Message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Minimal OpenAI-compatible responder: reads the request body, extracts the
    /// `"model"` field, and returns it verbatim as the completion content.
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

    fn tiered_config(base_url: String) -> LlmConfig {
        LlmConfig {
            cloud_api_keys: vec!["test-key".to_string()],
            cloud_base_url: base_url,
            cloud_model: "default-model".to_string(),
            tier_models: TierModels {
                fast: Some("fast-model".to_string()),
                medium: None,
                thinking: None,
            },
            ..LlmConfig::default()
        }
    }

    #[tokio::test]
    async fn complete_tiered_uses_the_configured_tier_model() {
        let base_url = spawn_model_echo_server().await;
        let router = LlmRouter::init(tiered_config(base_url)).await;

        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        let text = router.complete_tiered(Some(Tier::Fast), req).await.expect("completion");

        assert_eq!(text, "fast-model", "Some(configured tier) must route to its override model");
    }

    #[tokio::test]
    async fn complete_tiered_falls_back_to_default_when_tier_is_none() {
        let base_url = spawn_model_echo_server().await;
        let router = LlmRouter::init(tiered_config(base_url)).await;

        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        let text = router.complete_tiered(None, req).await.expect("completion");

        assert_eq!(text, "default-model", "tier=None must use the default model");
    }

    #[tokio::test]
    async fn complete_tiered_falls_back_to_default_when_tier_is_unconfigured() {
        let base_url = spawn_model_echo_server().await;
        let router = LlmRouter::init(tiered_config(base_url)).await;

        // `medium` has no override in `tiered_config` — must fall back, not error.
        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        let text = router.complete_tiered(Some(Tier::Medium), req).await.expect("completion");

        assert_eq!(text, "default-model", "tier=Some(unconfigured) must fall back to the default model");
    }

    #[tokio::test]
    async fn complete_tiered_propagates_primary_error_when_no_backend_configured() {
        // No cloud keys at all — primary is `NoopClient`, which always errors. Both
        // paths (tier and no-tier) must surface that same error, not panic or hang.
        let router = LlmRouter::init(LlmConfig {
            cloud_api_keys: vec![],
            ..LlmConfig::default()
        })
        .await;

        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        let err = router.complete_tiered(Some(Tier::Fast), req).await.expect_err("no backend configured");
        assert!(err.to_string().contains("Chưa cấu hình LLM"));
    }
}
