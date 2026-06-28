/// Embedded local inference via llama.cpp (requires `features = ["llama"]`).
///
/// Uses `tokio::task::spawn_blocking` to run the synchronous llama-cpp-2 inference
/// on a thread-pool thread, keeping the async executor unblocked.
///
/// Supports ChatML (Qwen2.5) and Gemma4 prompt formats via `PromptFormat`.
use crate::{
    prompt::{self, PromptFormat},
    CompletionRequest, LlmClient,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use encoding_rs::UTF_8;
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::params::LlamaModelParams,
    model::AddBos,
    model::LlamaModel,
    sampling::LlamaSampler,
};
use std::{num::NonZeroU32, path::PathBuf, sync::Arc};

pub struct LlamaClient {
    backend: Arc<LlamaBackend>,
    model: Arc<LlamaModel>,
    n_ctx: u32,
    fmt: PromptFormat,
}

impl LlamaClient {
    /// Load a GGUF model from disk. Blocks the calling thread while loading.
    ///
    /// `n_gpu_layers`: layers to offload to GPU (0 = CPU-only, 999 = full offload).
    /// llama.cpp clamps the value to the actual layer count of the model.
    pub fn load(model_path: PathBuf, n_ctx: u32, fmt: PromptFormat, n_gpu_layers: u32) -> Result<Self> {
        let backend = LlamaBackend::init().context("init llama backend")?;
        let model_params = LlamaModelParams::default()
            .with_n_gpu_layers(n_gpu_layers as i32);
        let model = LlamaModel::load_from_file(&backend, &model_path, &model_params)
            .context("load GGUF model")?;
        Ok(Self {
            backend: Arc::new(backend),
            model: Arc::new(model),
            n_ctx,
            fmt,
        })
    }
}

#[async_trait]
impl LlmClient for LlamaClient {
    async fn complete(&self, req: CompletionRequest) -> Result<String> {
        let prompt_text = self.fmt.format(&req.messages);
        let max_tokens = req.max_tokens.unwrap_or(1024) as i32;
        let temperature = req.temperature;
        let n_ctx = self.n_ctx;

        let backend = Arc::clone(&self.backend);
        let model = Arc::clone(&self.model);

        tokio::task::spawn_blocking(move || {
            run_inference(&backend, &model, &prompt_text, n_ctx, max_tokens, temperature)
        })
        .await
        .context("spawn_blocking panicked")?
    }

    fn provider_name(&self) -> &str {
        "llama.cpp"
    }
}

fn run_inference(
    backend: &LlamaBackend,
    model: &LlamaModel,
    prompt: &str,
    n_ctx: u32,
    max_new_tokens: i32,
    temperature: f32,
) -> Result<String> {
    // Tokenize
    let tokens = model
        .str_to_token(prompt, AddBos::Always)
        .context("tokenize prompt")?;

    if tokens.len() >= n_ctx as usize {
        return Err(anyhow!(
            "prompt ({} tokens) exceeds context window ({n_ctx})",
            tokens.len()
        ));
    }

    // Context
    let ctx_params = LlamaContextParams::default()
        .with_n_ctx(Some(NonZeroU32::new(n_ctx).unwrap()))
        .with_n_batch(512);
    let mut ctx = model
        .new_context(backend, ctx_params)
        .context("create llama context")?;

    // Fill initial batch
    let mut batch = LlamaBatch::new(tokens.len().max(512), 1);
    for (i, &token) in tokens.iter().enumerate() {
        let is_last = i == tokens.len() - 1;
        batch.add(token, i as i32, &[0], is_last)?;
    }
    ctx.decode(&mut batch).context("decode prompt batch")?;

    // Sampler chain: top-k → top-p → temperature
    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::top_k(40),
        LlamaSampler::top_p(0.9, 1),
        LlamaSampler::temp(temperature),
    ]);

    let mut output = String::new();
    let mut n_cur = tokens.len() as i32;
    // Stateful UTF-8 decoder — must live across the full generation loop.
    let mut decoder = UTF_8.new_decoder();

    loop {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);

        if model.is_eog_token(token) {
            break;
        }

        let piece = model
            .token_to_piece(token, &mut decoder, false, None)
            .context("decode token")?;
        output.push_str(&piece);

        batch.clear();
        batch.add(token, n_cur, &[0], true)?;
        ctx.decode(&mut batch).context("decode token batch")?;

        n_cur += 1;
        if n_cur - tokens.len() as i32 >= max_new_tokens {
            break;
        }
        if n_cur >= n_ctx as i32 {
            break;
        }
    }

    Ok(output)
}
