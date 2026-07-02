mod cloud;
mod prompt;
mod router;
mod sse;

#[cfg(feature = "llama")]
mod gpu;
#[cfg(feature = "llama")]
mod llama;

pub use cloud::CloudClient;
pub use prompt::PromptFormat;
pub use router::{LlmConfig, LlmRouter};

#[cfg(feature = "llama")]
pub use llama::LlamaClient;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: content.into() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: content.into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
    pub temperature: f32,
    pub tools: Option<Vec<serde_json::Value>>,
    /// Per-turn cancellation, consulted by `complete_stream` implementations only
    /// (`complete()` has no long-running loop to interrupt mid-flight, so it ignores
    /// this field entirely). `None` means "not cancellable" — every call site that
    /// doesn't have a real per-turn token (sub-turns, skill-synthesis workers) uses
    /// `CompletionRequest::simple`, which leaves this `None` rather than fabricating a
    /// token that can never fire.
    pub cancel: Option<CancellationToken>,
}

impl CompletionRequest {
    pub fn simple(messages: Vec<Message>) -> Self {
        Self { messages, max_tokens: Some(2048), temperature: 0.7, tools: None, cancel: None }
    }

    /// Attaches a per-turn cancellation token, consumed by streaming backends to abort
    /// generation early. Chainable so call sites read as
    /// `CompletionRequest::simple(msgs).with_cancel(token)`.
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }
}

/// A single unit of a streamed completion. Sent over a bounded `mpsc::Receiver` (see
/// `LlmClient::complete_stream`) — `Token` for each incremental piece of generated
/// text, `Done` exactly once on clean completion, `Error` on any failure (init,
/// mid-stream disconnect, or cancellation). After `Done`/`Error` the sender drops the
/// channel; consumers must stop reading on either variant, not just on channel close.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    Token(String),
    Done { total_tokens: u32 },
    Error(String),
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<String>;

    /// Streams raw model output as it's generated, over a bounded channel (backpressure:
    /// a slow consumer blocks the producer rather than letting memory grow unbounded).
    /// This is a dumb token pipe — no tool-call awareness. Callers that must withhold
    /// tool-call markup from the user (the agent loop's `run_turn`) wrap this with a
    /// buffering hold-back adapter; both backends stay simple push-producers.
    ///
    /// # Errors
    /// Returns `Err` only for init/pre-first-token failures (e.g. no backend
    /// configured, HTTP connect failure, prompt tokenization failure). Once the
    /// channel is returned, all further failures (mid-stream disconnect, cancellation)
    /// surface as `StreamChunk::Error` on the channel — never as a second `Err` and
    /// never as a silent retry that would duplicate already-streamed text.
    async fn complete_stream(&self, req: CompletionRequest) -> Result<mpsc::Receiver<StreamChunk>>;

    fn provider_name(&self) -> &str;

    /// Context window size (tokens) this backend can accept in a single prompt.
    /// Used by `haily-core`'s token budgeter to decide how much history fits —
    /// each backend reports its own so budgeting stays correct across a hot-swap
    /// (e.g. llama.cpp's configured `n_ctx` vs a cloud provider's much larger window).
    fn context_window(&self) -> u32;
}
