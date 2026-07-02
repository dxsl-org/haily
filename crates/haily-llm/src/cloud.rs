use crate::{prompt, CompletionRequest, LlmClient};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::sync::atomic::{AtomicUsize, Ordering};

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
        Ok(Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()?,
            base_url: base_url.into(),
            api_keys,
            model: model.into(),
            next_key_idx: AtomicUsize::new(0),
        })
    }

    /// Single attempt with one key. Returns `Ok(None)` on 429 (caller rotates),
    /// `Ok(Some(text))` on success, `Err` on any other failure.
    async fn try_key(&self, req: &CompletionRequest, key: &str) -> Result<Option<String>> {
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

        let resp = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await?;

        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            return Ok(None);
        }

        let parsed: ChatResponse = resp.error_for_status()?.json().await?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| anyhow!("cloud API returned no content"))?;
        Ok(Some(content))
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

        // Round-robin: pick a starting key, then rotate on 429.
        let start = self.next_key_idx.fetch_add(1, Ordering::Relaxed) % n;
        let last_err = anyhow!("tất cả API keys đều bị rate-limit (429)");

        for i in 0..n {
            let idx = (start + i) % n;
            let key = &self.api_keys[idx];
            match self.try_key(&req, key).await {
                Ok(Some(text)) => return Ok(text),
                Ok(None) => {
                    tracing::warn!(key_idx = idx, total = n, "API key 429 — thử key tiếp theo");
                }
                Err(e) => return Err(e), // non-429: do not rotate
            }
        }

        Err(last_err)
    }

    fn provider_name(&self) -> &str {
        "cloud"
    }
}
