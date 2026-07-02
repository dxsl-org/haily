use crate::CloudClient;
#[cfg(feature = "llama")]
use crate::prompt::PromptFormat;
use crate::{CompletionRequest, LlmClient, StreamChunk};
use anyhow::Result;
use async_trait::async_trait;
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

/// Runtime configuration for LLM routing.
/// Loaded from KMS preferences on startup; user can update without restart.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// One or more API keys for the cloud backend.
    /// Multiple keys rotate round-robin; empty = no cloud backend.
    pub cloud_api_keys: Vec<String>,
    pub cloud_base_url: String,
    pub cloud_model: String,

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

/// Routes requests to the best available LLM backend.
///
/// Priority:
///   1. llama.cpp embedded  (feature = "llama", model file present)
///   2. Cloud API           (api_key configured)
pub struct LlmRouter {
    primary: Arc<dyn LlmClient>,
    fallback: Option<Arc<dyn LlmClient>>,
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
                    },
                    Ok(Err(e)) => tracing::warn!("llama.cpp load failed: {e:#}"),
                    Err(e)     => tracing::warn!("llama.cpp spawn failed: {e:#}"),
                }
            }
        }

        if let Some(cloud_client) = cloud {
            tracing::info!("LLM: cloud API ({})", config.cloud_model);
            return Self { primary: cloud_client, fallback: None };
        }

        tracing::warn!("No LLM backend configured — open Settings → Model LLM");
        Self { primary: Arc::new(NoopClient), fallback: None }
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
