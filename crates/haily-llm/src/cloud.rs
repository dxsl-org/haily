use crate::{prompt, CompletionRequest, LlmClient};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

/// OpenAI-compatible chat completion client.
/// Covers: OpenAI, Anthropic (via proxy), Gemini (via proxy), local OpenAI-compat servers.
pub struct CloudClient {
    http: Client,
    base_url: String,
    api_key: String,
    model: String,
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
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client build"),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// Convenience constructor for OpenAI.
    pub fn openai(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::new("https://api.openai.com", api_key, model)
    }

    /// Convenience constructor for Anthropic via its OpenAI-compat endpoint.
    pub fn anthropic(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::new("https://api.anthropic.com", api_key, model)
    }
}

#[async_trait]
impl LlmClient for CloudClient {
    async fn complete(&self, req: CompletionRequest) -> Result<String> {
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
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let parsed: ChatResponse = resp.json().await?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| anyhow!("cloud API returned no content"))?;
        Ok(content)
    }

    fn provider_name(&self) -> &str {
        "cloud"
    }
}
