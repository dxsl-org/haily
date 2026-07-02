use crate::breaker::{Admission, CircuitBreaker};
use crate::sse::{self, Dialect, ParsedEvent};
use crate::{prompt, CompletionRequest, LlmClient, StreamChunk};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::Deserialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

/// Bounded channel size for the cloud streaming path — mirrors llama's bound in
/// spirit (backpressure against a slow consumer); cloud responses are already
/// network-rate-limited so a smaller bound is fine here.
const CLOUD_STREAM_BOUND: usize = 32;

/// Anthropic's required API version header — pinned to the version this client's
/// request/response shapes (`content_block_delta`, `text_delta`, etc.) were verified
/// against (see research report 01 sources).
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// OpenAI-compatible client with multi-key round-robin rotation.
///
/// On HTTP 429 (rate-limited) the current key is skipped and the next one is tried
/// within the same request. Non-429 errors propagate immediately without rotation.
pub struct CloudClient {
    http: Client,
    base_url: String,
    api_keys: Vec<String>,
    model: String,
    /// Monotonic counter; key selected by `counter % len`. Wraps naturally.
    next_key_idx: AtomicUsize,
    /// Per-key transport-failure breaker (index-aligned with `api_keys`). 429 is a
    /// routing signal, not a transport failure, and must never touch this — see
    /// `breaker` module docs for the full contract.
    breaker: CircuitBreaker,
}

/// Outcome of one key attempt, distinguishing the breaker-relevant transport failure
/// from an HTTP-status failure (auth error, malformed response, etc.) that reached
/// the server and therefore proves the key/network path is up.
enum KeyOutcome<T> {
    Success(T),
    RateLimited,
    /// Connection/timeout/DNS failure — never got an HTTP response at all.
    TransportError(anyhow::Error),
    /// Non-429 HTTP error or a malformed-but-received response body.
    HttpError(anyhow::Error),
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
}

impl CloudClient {
    /// # Errors
    /// Returns an error if the underlying `reqwest::Client` fails to build (e.g. TLS
    /// backend initialization failure) — propagated instead of panicking so the caller
    /// can fall back to another LLM backend.
    pub fn new(
        base_url: impl Into<String>,
        api_keys: Vec<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        let breaker = CircuitBreaker::new(api_keys.len());
        Ok(Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()?,
            base_url: base_url.into(),
            api_keys,
            model: model.into(),
            next_key_idx: AtomicUsize::new(0),
            breaker,
        })
    }

    /// Single attempt with one key. Distinguishes a connect/timeout failure (never
    /// reached the server — breaker-relevant) from an HTTP-status failure (server
    /// responded, so the network path and key transport are proven up) so the caller
    /// can report the correct outcome to the circuit breaker.
    async fn try_key(&self, req: &CompletionRequest, key: &str) -> KeyOutcome<String> {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": prompt::to_openai_messages(&req.messages),
            "temperature": req.temperature,
        });
        if let Some(max) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        if let Some(tools) = &req.tools {
            body["tools"] = serde_json::json!(tools);
        }

        let resp = match self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => return KeyOutcome::TransportError(e.into()),
        };

        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            return KeyOutcome::RateLimited;
        }

        let resp = match resp.error_for_status() {
            Ok(resp) => resp,
            Err(e) => return KeyOutcome::HttpError(e.into()),
        };
        let parsed: ChatResponse = match resp.json().await {
            Ok(p) => p,
            Err(e) => return KeyOutcome::HttpError(e.into()),
        };
        match parsed.choices.into_iter().next().and_then(|c| c.message.content) {
            Some(content) => KeyOutcome::Success(content),
            None => KeyOutcome::HttpError(anyhow!("cloud API returned no content")),
        }
    }

    fn dialect(&self) -> Dialect {
        Dialect::from_base_url(&self.base_url)
    }

    /// Builds the dialect-specific streaming request (endpoint, auth header, body).
    /// Both dialects set `"stream": true`; Anthropic additionally requires
    /// `max_tokens` (not optional there) and the `anthropic-version` header.
    fn build_stream_request(&self, req: &CompletionRequest, key: &str) -> RequestBuilder {
        match self.dialect() {
            Dialect::OpenAi => {
                let mut body = serde_json::json!({
                    "model": self.model,
                    "messages": prompt::to_openai_messages(&req.messages),
                    "temperature": req.temperature,
                    "stream": true,
                });
                if let Some(max) = req.max_tokens {
                    body["max_tokens"] = serde_json::json!(max);
                }
                if let Some(tools) = &req.tools {
                    body["tools"] = serde_json::json!(tools);
                }
                self.http
                    .post(format!("{}/v1/chat/completions", self.base_url))
                    .bearer_auth(key)
                    .json(&body)
            }
            Dialect::Anthropic => {
                let (system, messages) = prompt::to_anthropic_messages(&req.messages);
                let mut body = serde_json::json!({
                    "model": self.model,
                    "messages": messages,
                    "temperature": req.temperature,
                    "max_tokens": req.max_tokens.unwrap_or(1024),
                    "stream": true,
                });
                if let Some(system) = system {
                    body["system"] = serde_json::json!(system);
                }
                if let Some(tools) = &req.tools {
                    body["tools"] = serde_json::json!(tools);
                }
                self.http
                    .post(format!("{}/v1/messages", self.base_url))
                    .header("x-api-key", key)
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .json(&body)
            }
        }
    }

    /// Single streaming attempt with one key. 429 is a pre-stream HTTP status for
    /// both dialects (never an in-band SSE event) — detected from the response
    /// status before `.bytes_stream()` is ever called, so it rotates exactly like
    /// `try_key`'s non-streaming counterpart. `TransportError` covers connect/timeout
    /// failures before any response was received (breaker-relevant); `HttpError`
    /// covers a non-429 status from a server that did respond (not breaker-relevant).
    async fn try_key_stream(
        &self,
        req: &CompletionRequest,
        key: &str,
    ) -> KeyOutcome<mpsc::Receiver<StreamChunk>> {
        let resp = match self.build_stream_request(req, key).send().await {
            Ok(resp) => resp,
            Err(e) => return KeyOutcome::TransportError(e.into()),
        };

        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            return KeyOutcome::RateLimited;
        }
        let resp = match resp.error_for_status() {
            Ok(resp) => resp,
            Err(e) => return KeyOutcome::HttpError(e.into()),
        };

        let dialect = self.dialect();
        let cancel = req.cancel.clone().unwrap_or_default();
        let (tx, rx) = mpsc::channel(CLOUD_STREAM_BOUND);

        tokio::spawn(async move {
            let mut events = resp.bytes_stream().eventsource();
            let mut total_tokens: u32 = 0;
            loop {
                let next = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        let _ = tx.send(StreamChunk::Error("cancelled".to_string())).await;
                        return;
                    }
                    next = events.next() => next,
                };
                let Some(frame) = next else {
                    // Stream ended without a dialect-recognized Done marker — a mid-
                    // stream disconnect (network drop). Never auto-retry here: some
                    // output may already be user-visible, so a silent retry would
                    // duplicate it. Surface as Error and let the caller's turn fail.
                    let _ = tx.send(StreamChunk::Error("cloud stream ended unexpectedly (disconnected)".to_string())).await;
                    return;
                };
                let event = match frame {
                    Ok(event) => event,
                    Err(e) => {
                        let _ = tx.send(StreamChunk::Error(format!("SSE parse error: {e}"))).await;
                        return;
                    }
                };
                match sse::parse_event(dialect, &event) {
                    ParsedEvent::Delta(text) => {
                        total_tokens += 1;
                        if tx.send(StreamChunk::Token(text)).await.is_err() {
                            return; // receiver dropped
                        }
                    }
                    ParsedEvent::Done => {
                        let _ = tx.send(StreamChunk::Done { total_tokens }).await;
                        return;
                    }
                    ParsedEvent::Error(msg) => {
                        let _ = tx.send(StreamChunk::Error(msg)).await;
                        return;
                    }
                    ParsedEvent::Ignore => {}
                }
            }
        });

        KeyOutcome::Success(rx)
    }
}

#[async_trait]
impl LlmClient for CloudClient {
    async fn complete(&self, req: CompletionRequest) -> Result<String> {
        let n = self.api_keys.len();
        if n == 0 {
            return Err(anyhow!(
                "Chưa cấu hình API key. Mở Settings → Cloud API để thêm key."
            ));
        }

        // Round-robin: pick a starting key, then rotate on 429 or an open breaker.
        let start = self.next_key_idx.fetch_add(1, Ordering::Relaxed) % n;
        let last_err = anyhow!("tất cả API keys đều bị rate-limit hoặc tạm ngưng (breaker mở)");

        for i in 0..n {
            let idx = (start + i) % n;
            if self.breaker.try_acquire(idx) == Admission::Blocked {
                tracing::warn!(key_idx = idx, total = n, "API key breaker open — bỏ qua");
                continue;
            }
            let key = &self.api_keys[idx];
            match self.try_key(&req, key).await {
                KeyOutcome::Success(text) => {
                    self.breaker.record_success(idx);
                    return Ok(text);
                }
                KeyOutcome::RateLimited => {
                    // 429 is a routing signal, not a transport failure — must not
                    // trip or close the breaker (would blacklist a key that is
                    // merely busy). Only releases a held probe slot, if any.
                    self.breaker.record_inconclusive(idx);
                    tracing::warn!(key_idx = idx, total = n, "API key 429 — thử key tiếp theo");
                }
                KeyOutcome::TransportError(e) => {
                    self.breaker.record_failure(idx);
                    return Err(e); // non-429: do not rotate
                }
                KeyOutcome::HttpError(e) => {
                    // Server responded (proves the transport path is up) but the
                    // request itself failed — releases any held probe slot without
                    // touching the failure streak, then propagates per existing
                    // non-429 semantics (no rotation).
                    self.breaker.record_inconclusive(idx);
                    return Err(e);
                }
            }
        }

        Err(last_err)
    }

    /// Streams the completion via SSE. 429 is checked pre-stream and rotates keys
    /// exactly like `complete()`; once a key's response streams, this method's `Err`
    /// path is exhausted (per the trait's fallback-scope contract) — anything that
    /// goes wrong after the SSE forwarder task is spawned surfaces as
    /// `StreamChunk::Error` on the returned channel instead.
    async fn complete_stream(&self, req: CompletionRequest) -> Result<mpsc::Receiver<StreamChunk>> {
        let n = self.api_keys.len();
        if n == 0 {
            return Err(anyhow!(
                "Chưa cấu hình API key. Mở Settings → Cloud API để thêm key."
            ));
        }

        let start = self.next_key_idx.fetch_add(1, Ordering::Relaxed) % n;
        let last_err = anyhow!("tất cả API keys đều bị rate-limit hoặc tạm ngưng (breaker mở)");

        for i in 0..n {
            let idx = (start + i) % n;
            if self.breaker.try_acquire(idx) == Admission::Blocked {
                tracing::warn!(key_idx = idx, total = n, "API key breaker open (stream) — bỏ qua");
                continue;
            }
            let key = &self.api_keys[idx];
            match self.try_key_stream(&req, key).await {
                KeyOutcome::Success(rx) => {
                    self.breaker.record_success(idx);
                    return Ok(rx);
                }
                KeyOutcome::RateLimited => {
                    self.breaker.record_inconclusive(idx);
                    tracing::warn!(key_idx = idx, total = n, "API key 429 (stream) — thử key tiếp theo");
                }
                KeyOutcome::TransportError(e) => {
                    self.breaker.record_failure(idx);
                    return Err(e); // non-429: do not rotate
                }
                KeyOutcome::HttpError(e) => {
                    self.breaker.record_inconclusive(idx);
                    return Err(e); // non-429: do not rotate
                }
            }
        }

        Err(last_err)
    }

    fn provider_name(&self) -> &str {
        "cloud"
    }

    fn context_window(&self) -> u32 {
        crate::router::CLOUD_CONTEXT_WINDOW_CLAMP
    }
}
