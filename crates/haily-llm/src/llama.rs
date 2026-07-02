/// Embedded local inference via llama.cpp (requires `features = ["llama"]`).
///
/// Uses `tokio::task::spawn_blocking` to run the synchronous llama-cpp-2 inference
/// on a thread-pool thread, keeping the async executor unblocked.
///
/// Supports ChatML (Qwen2.5) and Gemma4 prompt formats via `PromptFormat`.
use crate::{
    prompt::PromptFormat,
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
use std::{num::NonZeroU32, path::PathBuf, sync::Arc, sync::OnceLock};

/// Process-global llama backend. llama.cpp's backend is a singleton guarded by a
/// global flag: `LlamaBackend::init()` errors if called while another backend is
/// alive, and dropping it frees global state. Re-initializing across a model
/// reload therefore fails (→ silent fallback) or aborts the process mid-inference.
///
/// We initialize it exactly once and never drop it, so every `LlamaClient` —
/// including ones created by a hot model swap — shares the same valid backend.
static BACKEND: OnceLock<LlamaBackend> = OnceLock::new();

/// Returns the shared backend, initializing it on first use.
///
/// Only one `LlamaBackend::init()` can succeed process-wide; concurrent callers
/// that lose the race spin briefly until the winner publishes the backend.
fn backend() -> Result<&'static LlamaBackend> {
    if let Some(b) = BACKEND.get() {
        return Ok(b);
    }
    match LlamaBackend::init() {
        Ok(b) => {
            // set() only fails if another thread published first; in that case our
            // `b` is the unique backend and theirs cannot exist, so this never errs.
            let _ = BACKEND.set(b);
            Ok(BACKEND.get().expect("backend just set"))
        }
        Err(_already_init) => {
            // Another thread is mid-init; wait for it to publish.
            loop {
                if let Some(b) = BACKEND.get() {
                    return Ok(b);
                }
                std::thread::yield_now();
            }
        }
    }
}

pub struct LlamaClient {
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
        let backend = backend()?;
        // Requesting GPU offload on a build with no GPU backend makes llama.cpp try
        // to allocate buffers on a non-existent device — an abort that kills the whole
        // process, not a recoverable error. Clamp to CPU-only when GPU is unavailable.
        let effective_gpu_layers = if n_gpu_layers > 0 && !backend.supports_gpu_offload() {
            tracing::warn!(
                requested = n_gpu_layers,
                "no GPU backend compiled/available — forcing CPU-only (n_gpu_layers=0)"
            );
            0
        } else {
            n_gpu_layers
        };
        let model_params = LlamaModelParams::default()
            .with_n_gpu_layers(effective_gpu_layers);
        let model = LlamaModel::load_from_file(backend, &model_path, &model_params)
            .context("load GGUF model")?;
        Ok(Self {
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

        let model = Arc::clone(&self.model);

        let fmt = self.fmt;
        let raw = tokio::task::spawn_blocking(move || {
            let backend = backend()?;
            run_inference(backend, &model, &prompt_text, n_ctx, max_tokens, temperature)
        })
        .await
        .context("spawn_blocking panicked")??;

        // Strip trailing stop tokens that weren't caught by is_eog_token().
        // llama.cpp's EOG detection is vocabulary-dependent and may miss stop
        // sequences that the model generates as plain text pieces.
        let stop: &[&str] = match fmt {
            PromptFormat::ChatML => &["<|im_end|>"],
            PromptFormat::Gemma4 => &["<end_of_turn>", "</start_of_turn>"],
        };
        let mut out = raw.as_str();
        for seq in stop {
            if let Some(stripped) = out.strip_suffix(seq) {
                out = stripped;
            }
        }
        Ok(out.trim_end().to_string())
    }

    fn provider_name(&self) -> &str {
        "llama.cpp"
    }
}

/// Validate `n_ctx` is a usable (non-zero) context window size.
///
/// Extracted as a pure function so the n_ctx=0 misconfiguration path is unit-testable
/// without a loaded GGUF model (`run_inference` needs a real `LlamaModel`/`LlamaBackend`).
fn validate_n_ctx(n_ctx: u32) -> Result<NonZeroU32> {
    NonZeroU32::new(n_ctx)
        .ok_or_else(|| anyhow!("llama.cpp context window (n_ctx) must be non-zero"))
}

fn run_inference(
    backend: &LlamaBackend,
    model: &LlamaModel,
    prompt: &str,
    n_ctx: u32,
    max_new_tokens: i32,
    temperature: f32,
) -> Result<String> {
    // n_ctx=0 is a misconfiguration (empty context window) — reject it up front with a
    // clear error instead of panicking inside NonZeroU32::new(..).unwrap() below.
    let n_ctx_nonzero = validate_n_ctx(n_ctx)?;

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

    // Context. n_batch must be ≥ the prompt length: the prompt is decoded in a
    // single batch below, and llama.cpp asserts `n_tokens_all <= n_batch`
    // (an abort, not a recoverable error). The full system prompt + tool reference
    // routinely exceeds the old 512 default, so size the batch to the context.
    let ctx_params = LlamaContextParams::default()
        .with_n_ctx(Some(n_ctx_nonzero))
        .with_n_batch(n_ctx);
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

    // Sampler chain MUST end with a selecting sampler (`dist`/`greedy`); the
    // transform samplers (top_k/top_p/temp) only reshape logits and never set
    // the selected token, leaving `llama_sampler_sample` to return a garbage
    // token (observed as immediate EOG → empty output).
    let mut sampler = if temperature <= 0.0 {
        LlamaSampler::chain_simple([LlamaSampler::greedy()])
    } else {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        LlamaSampler::chain_simple([
            LlamaSampler::top_k(40),
            LlamaSampler::top_p(0.9, 1),
            LlamaSampler::temp(temperature),
            LlamaSampler::dist(seed),
        ])
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_n_ctx_rejects_zero() {
        assert!(validate_n_ctx(0).is_err());
    }

    #[test]
    fn validate_n_ctx_accepts_positive_values() {
        let n = validate_n_ctx(4096).unwrap();
        assert_eq!(n.get(), 4096);
    }
}
