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
        // chars/3 heuristic from `haily_core::budget::estimate` — duplicated here (not
        // imported: haily-llm is a leaf crate and must not depend on haily-core) purely
        // to log estimate-vs-actual so the heuristic's accuracy can be validated against
        // this call's real tokenize() count (research report 03 §A2 risk note).
        let estimated_prompt_tokens = prompt_text.chars().count().div_ceil(3);
        let max_tokens = req.max_tokens.unwrap_or(1024) as i32;
        let temperature = req.temperature;
        let n_ctx = self.n_ctx;

        let model = Arc::clone(&self.model);

        let fmt = self.fmt;
        let (raw, actual_prompt_tokens) = tokio::task::spawn_blocking(move || {
            let backend = backend()?;
            run_inference(backend, &model, &prompt_text, n_ctx, max_tokens, temperature)
        })
        .await
        .context("spawn_blocking panicked")??;

        tracing::debug!(
            estimated = estimated_prompt_tokens,
            actual = actual_prompt_tokens,
            "prompt token estimate vs actual"
        );

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

    fn context_window(&self) -> u32 {
        self.n_ctx
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

/// Ceiling on `n_batch` — the physical-token compute buffer llama.cpp allocates
/// up front. Decoupled from `n_ctx` (phase-05): before this, `n_batch` was set
/// equal to `n_ctx`, so raising the context window (e.g. 4096 → 8192) silently
/// doubled the compute buffer too, even though `n_batch` only bounds how many
/// tokens are decoded in a single `llama_decode` call, not the context size.
const N_BATCH_CEILING: u32 = 512;

/// Size `n_batch` for a single-shot prompt decode.
///
/// The prompt is decoded in ONE batch (see `run_inference` below), and llama.cpp
/// asserts `n_tokens_all <= n_batch` inside `llama_decode` — a hard process abort,
/// not a recoverable `Result::Err`, if violated (confirmed by a prior incident: commit
/// b958477, GGML_ASSERT(n_tokens <= n_batch) aborting on the first message once the
/// system prompt + tool reference exceeded the old flat 512 default). So `n_batch`
/// must never be set below `prompt_tokens`, but it also should not scale with
/// `n_ctx` — those are independent llama.cpp knobs. Pure function so both the
/// ceiling and the safety floor are unit-testable without a loaded model.
fn compute_n_batch(prompt_tokens: usize, n_ctx: u32) -> u32 {
    (prompt_tokens as u32).max(N_BATCH_CEILING).min(n_ctx)
}

fn run_inference(
    backend: &LlamaBackend,
    model: &LlamaModel,
    prompt: &str,
    n_ctx: u32,
    max_new_tokens: i32,
    temperature: f32,
) -> Result<(String, usize)> {
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

    // n_batch sized to the actual prompt (see `compute_n_batch`), NOT to n_ctx —
    // raising the context window must not silently double the batch compute buffer.
    let n_batch = compute_n_batch(tokens.len(), n_ctx);
    let ctx_params = LlamaContextParams::default()
        .with_n_ctx(Some(n_ctx_nonzero))
        .with_n_batch(n_batch);
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

    Ok((output, tokens.len()))
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

    #[test]
    fn compute_n_batch_never_falls_below_prompt_length() {
        // A prompt bigger than the 512 ceiling must still get a big enough batch to
        // decode in one shot (this is the exact scenario that caused the b958477
        // process-abort regression before n_batch was tied to n_ctx).
        assert_eq!(compute_n_batch(2000, 8192), 2000);
    }

    #[test]
    fn compute_n_batch_is_capped_at_ceiling_for_small_prompts() {
        // Small prompt: batch should not balloon to n_ctx just because n_ctx is large —
        // this is the decoupling the phase-05 fix introduces.
        assert_eq!(compute_n_batch(50, 8192), 512);
    }

    #[test]
    fn compute_n_batch_never_exceeds_n_ctx() {
        // n_batch is meaningless (and rejected by llama.cpp) if it exceeds the
        // context window itself — clamp as a final safety bound.
        assert_eq!(compute_n_batch(2000, 1024), 1024);
    }

    #[test]
    fn compute_n_batch_does_not_double_when_n_ctx_doubles() {
        // The core decoupling assertion: raising n_ctx from 4096 to 8192 for a
        // moderate prompt must not double the batch compute buffer.
        let small_prompt = 300;
        assert_eq!(compute_n_batch(small_prompt, 4096), 512);
        assert_eq!(compute_n_batch(small_prompt, 8192), 512);
    }
}
