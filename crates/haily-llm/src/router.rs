use crate::CloudClient;
#[cfg(feature = "llama")]
use crate::prompt::PromptFormat;
use crate::{CompletionRequest, LlmClient};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

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

    fn provider_name(&self) -> &str {
        self.primary.provider_name()
    }

    fn context_window(&self) -> u32 {
        self.primary.context_window()
    }
}
