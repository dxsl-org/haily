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
}

#[cfg(feature = "llama")]
use crate::LlamaClient;

/// Runtime configuration for LLM routing.
/// Loaded from KMS preferences on startup; user can update without restart.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub cloud_api_key: Option<String>,
    pub cloud_base_url: String,
    pub cloud_model: String,

    /// Path to GGUF model file for embedded inference (only used with `llama` feature).
    #[cfg(feature = "llama")]
    pub llama_model_path: Option<std::path::PathBuf>,
    /// Context window size for llama.cpp (default 4096).
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
        Self {
            cloud_api_key: std::env::var("OPENAI_API_KEY").ok(),
            cloud_base_url: "https://api.openai.com".into(),
            cloud_model: "gpt-4o-mini".into(),
            #[cfg(feature = "llama")]
            llama_model_path: None,
            #[cfg(feature = "llama")]
            llama_n_ctx: 4096,
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
        let cloud: Option<Arc<dyn LlmClient>> = config.cloud_api_key.as_deref().map(|key| {
            Arc::new(CloudClient::new(
                config.cloud_base_url.clone(),
                key,
                config.cloud_model.clone(),
            )) as Arc<dyn LlmClient>
        });

        #[cfg(feature = "llama")]
        {
            let llama_ok = config.llama_model_path.as_ref()
                .map(|p| p.exists())
                .unwrap_or(false);

            if llama_ok {
                let path = config.llama_model_path.clone().unwrap();
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
}
