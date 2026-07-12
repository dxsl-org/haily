/// Embedded local inference via llama.cpp (requires `features = ["llama"]`).
///
/// Uses `tokio::task::spawn_blocking` to run the synchronous llama-cpp-2 inference
/// on a thread-pool thread, keeping the async executor unblocked.
///
/// Supports ChatML (Qwen2.5) and Gemma4 prompt formats via `PromptFormat`.
use crate::{
    prompt::PromptFormat,
    CompletionRequest, LlmClient, StreamChunk,
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
use tokio::sync::mpsc;

/// Vietnamese-fitted chars-per-token divisor for `complete()`'s prompt-token
/// estimate-vs-actual debug log — replaces the earlier flat `chars/3` heuristic
/// (phase-11, m4).
///
/// PROVENANCE CONTRACT: this value MUST be derived from `(char_count,
/// actual_prompt_tokens)` pairs collected on the STREAMING path
/// (`complete_stream` → `run_inference_streaming`'s returned prompt-token count →
/// `StreamChunk::Done.prompt_tokens`, wired in Phase 8/C2) — NEVER from
/// `complete()`'s own pairs, since `complete()` is the sub-turn-only path (main
/// user turns stream) and fitting from it would be circular. See
/// `.agents/260706-0952-activate-and-measure/reports/vn-tokenizer-divisor.md` for
/// the measurement procedure and this value's current provenance.
///
/// INTERIM VALUE (2026-07-06): the `llama` feature compiles in the phase-11
/// implementation environment (llama-cpp-2's native build succeeds), but no GGUF
/// model file was available to actually load and run inference, so the real
/// STREAMING measurement (driving real turns through `complete_stream` and
/// recording `StreamChunk::Done.prompt_tokens`) could not be executed. `2.5` is
/// therefore a reasoned placeholder — NOT a fitted empirical value —
/// grounded in published BPE-tokenizer behavior for Vietnamese (see the report:
/// diacritic-heavy Vietnamese text tokenizes denser than English on
/// English/Chinese-majority multilingual vocabularies like Qwen2.5's/Gemma's,
/// commonly cited in the ~2–3 chars/token range vs English's ~4). This constant is
/// a LOGGING-ONLY heuristic — it never gates context-window or batch sizing (those
/// use the real `tokens.len()` from `str_to_token`, see `run_inference_streaming`
/// below) — so an imprecise interim value has no behavioral effect beyond one
/// debug-log line's accuracy. Replace with the empirical fit once a llama-enabled
/// environment can run the measurement procedure in the report above.
const VN_PROMPT_CHARS_PER_TOKEN: f64 = 2.5;

/// Estimate a prompt's token count from its character count using
/// `VN_PROMPT_CHARS_PER_TOKEN`. Extracted as a pure function (mirrors
/// `validate_n_ctx`/`compute_n_batch` below) so the divisor's arithmetic is
/// unit-testable without a loaded GGUF model.
fn estimate_prompt_tokens(char_count: usize) -> usize {
    (char_count as f64 / VN_PROMPT_CHARS_PER_TOKEN).ceil() as usize
}

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
        // Estimate-vs-actual log only (research report 03 §A2 risk note) — not
        // imported from `haily_core::budget::estimate` since haily-llm is a leaf
        // crate and must not depend on haily-core; this is a narrower, local
        // instance of the same heuristic, purely for this call's debug log below.
        // Divisor provenance: see `VN_PROMPT_CHARS_PER_TOKEN`'s doc comment.
        let estimated_prompt_tokens = estimate_prompt_tokens(prompt_text.chars().count());
        let max_tokens = req.max_tokens.unwrap_or(1024) as i32;
        let temperature = req.temperature;
        let n_ctx = self.n_ctx;
        let grammar = req.grammar.clone();

        let model = Arc::clone(&self.model);

        let fmt = self.fmt;
        let (raw, actual_prompt_tokens) = tokio::task::spawn_blocking(move || {
            let backend = backend()?;
            run_inference(
                backend,
                &model,
                &prompt_text,
                n_ctx,
                max_tokens,
                temperature,
                grammar.as_deref(),
            )
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

    /// Streams token pieces as they're generated. Cancellation: `req.cancel` is
    /// checked once per token (right where `run_inference_streaming`'s `on_piece`
    /// callback fires) — firing it stops generation within one token's decode time.
    /// The bounded channel itself is a second, implicit cancellation path: if the
    /// consumer drops `rx` (e.g. it already saw `StreamChunk::Error` and gave up),
    /// `blocking_send` starts failing and the same `on_piece` callback stops the loop.
    async fn complete_stream(&self, req: CompletionRequest) -> Result<mpsc::Receiver<StreamChunk>> {
        let (tx, rx) = mpsc::channel(LLAMA_STREAM_BOUND);
        let cancel = req.cancel.clone().unwrap_or_default();
        let model = Arc::clone(&self.model);
        let fmt = self.fmt;
        let n_ctx = self.n_ctx;
        let prompt_text = self.fmt.format(&req.messages);
        let max_tokens = req.max_tokens.unwrap_or(1024) as i32;
        let temperature = req.temperature;
        let grammar = req.grammar.clone();

        tokio::task::spawn_blocking(move || {
            let stop: &[&str] = match fmt {
                PromptFormat::ChatML => &["<|im_end|>"],
                PromptFormat::Gemma4 => &["<end_of_turn>", "</start_of_turn>"],
            };
            // Belt-and-suspenders stop-sequence hold-back: the same fallback
            // `complete()` applies after the fact, but streaming must not emit the
            // stop sequence's bytes to the user piece-by-piece as they arrive, so a
            // small trailing buffer is withheld until it's provably not a stop
            // sequence prefix — mirrors `tag_matcher::holdback_len`'s shape (haily-llm
            // is a leaf crate and cannot depend on haily-core, so this is a narrower,
            // local instance of the same pattern, not a shared abstraction).
            let mut holdback = String::new();
            let mut total_tokens: u32 = 0;

            // C2 (Phase 8): `run_inference_streaming` returns `(output, prompt_token_count)`
            // — the SAME tokenize()-backed count `complete()` logs as `actual_prompt_tokens`
            // (line ~124 above). Captured here so the streaming path — the one MAIN user
            // turns actually use (`complete()` is sub-turn-only) — can finally emit a real
            // prompt-token measurement instead of none at all.
            let result = (|| -> Result<usize> {
                let backend = backend()?;
                run_inference_streaming(
                    backend,
                    &model,
                    &prompt_text,
                    n_ctx,
                    max_tokens,
                    temperature,
                    grammar.as_deref(),
                    |piece| {
                        if cancel.is_cancelled() {
                            return false;
                        }
                        total_tokens += 1;
                        holdback.push_str(piece);
                        let safe_len = stop_safe_prefix_len(&holdback, stop);
                        if safe_len > 0 {
                            let emit: String = holdback.drain(..safe_len).collect();
                            if tx.blocking_send(StreamChunk::Token(emit)).is_err() {
                                return false; // receiver dropped — stop generating
                            }
                        }
                        true
                    },
                )
                .map(|(_output, prompt_tokens)| prompt_tokens)
            })();

            match result {
                Ok(prompt_tokens) => {
                    if cancel.is_cancelled() {
                        let _ = tx.blocking_send(StreamChunk::Error("cancelled".to_string()));
                    } else {
                        // Whatever remains in `holdback` at clean end-of-generation was
                        // never confirmed to be a live stop sequence — flush it so no
                        // trailing text is silently dropped.
                        if !holdback.is_empty() {
                            let _ = tx.blocking_send(StreamChunk::Token(holdback));
                        }
                        let _ = tx.blocking_send(StreamChunk::Done {
                            total_tokens,
                            prompt_tokens: Some(prompt_tokens as u32),
                        });
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(StreamChunk::Error(format!("{e:#}")));
                }
            }
        });

        Ok(rx)
    }

    fn provider_name(&self) -> &str {
        "llama.cpp"
    }

    fn context_window(&self) -> u32 {
        self.n_ctx
    }
}

/// Bounded channel size for llama.cpp's streaming decode loop — gives backpressure
/// against a slow consumer (GUI/CLI/Telegram) without letting local inference (the
/// expensive resource) run unbounded ahead of what's been delivered.
const LLAMA_STREAM_BOUND: usize = 64;

/// Length (bytes) of the prefix of `holdback` that is safe to emit immediately —
/// i.e. the remaining tail cannot be extended into any of `stop` by appending more
/// characters. Returns 0 if the ENTIRE `holdback` is currently a prefix of some stop
/// sequence (must wait for more tokens, or for generation to end, to resolve).
///
/// Walks char-boundary start positions from the earliest (longest possible safe
/// emit) to latest, returning the FIRST (leftmost/longest-tail) boundary whose
/// remaining suffix is a stop-sequence prefix — that suffix, and everything after
/// it, must be withheld. Char boundaries only (not raw byte offsets): the `stop`
/// sequences are ASCII, but `holdback` itself can contain multibyte model output, so
/// an arbitrary byte offset could land mid-codepoint and panic.
fn stop_safe_prefix_len(holdback: &str, stop: &[&str]) -> usize {
    for take in holdback.char_indices().map(|(i, _)| i) {
        let candidate_tail = &holdback[take..];
        if stop.iter().any(|s| s.starts_with(candidate_tail)) {
            return take;
        }
    }
    holdback.len()
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
    grammar: Option<&str>,
) -> Result<(String, usize)> {
    // The non-streaming path never stops early (callback always returns `true` =
    // "keep going"), so the full output always matches what streaming would have
    // emitted piece-by-piece — one decode loop, two ways to consume it.
    run_inference_streaming(
        backend,
        model,
        prompt,
        n_ctx,
        max_new_tokens,
        temperature,
        grammar,
        |_piece| true,
    )
}

/// Same decode loop as `run_inference`, but invokes `on_piece(piece)` for every
/// generated token piece as it's produced. `on_piece` returns `false` to request an
/// early stop (used by the streaming path to react to cancellation or a dropped
/// receiver) — checked once per token, immediately after sampling, the same place
/// `is_eog_token` is already checked, so cancellation latency is bounded by one
/// token's decode time.
///
/// Returns `(full_output, prompt_token_count)` regardless of whether generation ran
/// to completion or stopped early — a caller that stopped early via `on_piece`
/// returning `false` still gets whatever was generated up to that point.
#[allow(clippy::too_many_arguments)] // one cohesive decode call; splitting into a params
// struct would obscure the llama.cpp call shape more than the arg count clarifies it.
fn run_inference_streaming(
    backend: &LlamaBackend,
    model: &LlamaModel,
    prompt: &str,
    n_ctx: u32,
    max_new_tokens: i32,
    temperature: f32,
    grammar: Option<&str>,
    mut on_piece: impl FnMut(&str) -> bool,
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
    //
    // GBNF (Phase 3): an optional grammar sampler is prepended so it masks logits to
    // grammar-legal tokens BEFORE the transform/selecting samplers run. Fallback
    // contract: if grammar-sampler construction fails (unsupported build, malformed
    // grammar, null bytes) we log::warn and generate UNCONSTRAINED rather than panic or
    // fail the call — the forced-JSON contracts must survive GBNF being unavailable.
    let mut samplers: Vec<LlamaSampler> = Vec::new();
    if let Some(gbnf) = grammar {
        match LlamaSampler::grammar(model, gbnf, "root") {
            Ok(grammar_sampler) => samplers.push(grammar_sampler),
            Err(e) => tracing::warn!(
                "GBNF grammar sampler init failed ({e}); falling back to unconstrained generation"
            ),
        }
    }
    if temperature <= 0.0 {
        samplers.push(LlamaSampler::greedy());
    } else {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        samplers.push(LlamaSampler::top_k(40));
        samplers.push(LlamaSampler::top_p(0.9, 1));
        samplers.push(LlamaSampler::temp(temperature));
        samplers.push(LlamaSampler::dist(seed));
    }
    let mut sampler = LlamaSampler::chain_simple(samplers);

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

        // Checked right after producing the piece, next to the existing is_eog_token
        // check — a `false` return (cancellation, or the streaming receiver was
        // dropped) stops generation immediately rather than continuing to burn CPU
        // on tokens nobody will see.
        if !on_piece(&piece) {
            break;
        }

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
    fn estimate_prompt_tokens_zero_chars_is_zero_tokens() {
        assert_eq!(estimate_prompt_tokens(0), 0);
    }

    #[test]
    fn estimate_prompt_tokens_rounds_up_to_the_next_whole_token() {
        // 2.5 chars/token: 5 chars → exactly 2 tokens; 6 chars → ceil(2.4) = 3.
        assert_eq!(estimate_prompt_tokens(5), 2);
        assert_eq!(estimate_prompt_tokens(6), 3);
    }

    #[test]
    fn estimate_prompt_tokens_scales_with_the_named_divisor() {
        // Locks the estimate to `VN_PROMPT_CHARS_PER_TOKEN` itself (not a hardcoded
        // literal) so this test still passes once the interim value is replaced
        // with the empirical fit from the streaming-pairs measurement.
        let chars = 1000;
        let expected = (chars as f64 / VN_PROMPT_CHARS_PER_TOKEN).ceil() as usize;
        assert_eq!(estimate_prompt_tokens(chars), expected);
    }

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

    const CHATML_STOP: &[&str] = &["<|im_end|>"];

    #[test]
    fn stop_safe_prefix_emits_everything_when_no_partial_stop_sequence() {
        assert_eq!(stop_safe_prefix_len("hello world", CHATML_STOP), "hello world".len());
    }

    #[test]
    fn stop_safe_prefix_withholds_entire_buffer_when_it_is_a_stop_prefix() {
        assert_eq!(stop_safe_prefix_len("<|im_end", CHATML_STOP), 0);
    }

    #[test]
    fn stop_safe_prefix_withholds_only_the_trailing_partial_match() {
        let holdback = "answer text<|im_e";
        let safe = stop_safe_prefix_len(holdback, CHATML_STOP);
        assert_eq!(&holdback[..safe], "answer text");
    }

    #[test]
    fn stop_safe_prefix_emits_everything_once_stop_sequence_diverges() {
        // "<|im_endX" cannot extend into "<|im_end|>" — safe to emit in full.
        assert_eq!(stop_safe_prefix_len("hi <|im_endX", CHATML_STOP), "hi <|im_endX".len());
    }

    #[test]
    fn stop_safe_prefix_handles_multibyte_text_before_the_partial_match() {
        let holdback = "xin chào <|im_e";
        let safe = stop_safe_prefix_len(holdback, CHATML_STOP);
        assert_eq!(&holdback[..safe], "xin chào ");
    }
}
