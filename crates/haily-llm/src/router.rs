use crate::{CloudClient, CompletionRequest, LlmClient, OllamaClient};
#[cfg(feature = "llama")]
use crate::prompt::PromptFormat;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::sync::Arc;

#[cfg(feature = "llama")]
use crate::LlamaClient;

/// Runtime configuration for LLM routing.
/// Loaded from KMS preferences on startup; user can update without restart.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Prefer Ollama over embedded llama.cpp when both are available.
    pub prefer_ollama: bool,
    pub ollama_url: String,
    pub ollama_model: String,

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
            prefer_ollama: false,
            ollama_url: "http://localhost:11434".into(),
            ollama_model: "qwen2.5:3b".into(),
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
///   1. llama.cpp embedded  (feature = "llama", model file present, prefer_ollama = false)
///   2. Ollama              (running at ollama_url, or prefer_ollama = true)
///   3. Cloud API           (api_key configured)
pub struct LlmRouter {
    primary: Arc<dyn LlmClient>,
    fallback: Option<Arc<dyn LlmClient>>,
}

impl LlmRouter {
    pub async fn init(config: LlmConfig) -> Result<Self> {
        let cloud: Option<Arc<dyn LlmClient>> = config.cloud_api_key.as_deref().map(|key| {
            Arc::new(CloudClient::new(
                config.cloud_base_url.clone(),
                key,
                config.cloud_model.clone(),
            )) as Arc<dyn LlmClient>
        });

        let ollama_running = OllamaClient::probe(&config.ollama_url).await;

        #[cfg(feature = "llama")]
        {
            let llama_ok = config.llama_model_path.as_ref()
                .map(|p| p.exists())
                .unwrap_or(false);

            if llama_ok && !config.prefer_ollama {
                let path = config.llama_model_path.clone().unwrap();
                tracing::info!("LLM: llama.cpp embedded ({})", path.display());
                let fmt = config.llama_prompt_format;
                let n_gpu_layers = config.llama_n_gpu_layers;
                tracing::info!(
                    "LLM: llama.cpp embedded ({}) — {}",
                    path.display(),
                    crate::gpu::gpu_mode_label(n_gpu_layers)
                );
                let client = tokio::task::spawn_blocking(move || {
                    LlamaClient::load(path, config.llama_n_ctx, fmt, n_gpu_layers)
                })
                .await??;
                return Ok(Self {
                    primary: Arc::new(client),
                    fallback: cloud,
                });
            }
        }

        if ollama_running {
            tracing::info!("LLM: Ollama @ {} ({})", config.ollama_url, config.ollama_model);
            let client = Arc::new(OllamaClient::new(&config.ollama_url, &config.ollama_model));
            return Ok(Self { primary: client, fallback: cloud });
        }

        if let Some(cloud_client) = cloud {
            tracing::info!("LLM: cloud API ({})", config.cloud_model);
            return Ok(Self { primary: cloud_client, fallback: None });
        }

        Err(anyhow!(
            "No LLM backend available. Options:\n\
             1. Install Ollama: https://ollama.com  →  ollama pull qwen2.5:3b\n\
             2. Set OPENAI_API_KEY env var for cloud fallback\n\
             3. Build with --features llama and provide a GGUF model file"
        ))
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
