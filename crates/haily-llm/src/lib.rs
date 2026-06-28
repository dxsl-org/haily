mod cloud;
mod ollama;
mod prompt;
mod router;

#[cfg(feature = "llama")]
mod gpu;
#[cfg(feature = "llama")]
mod llama;

pub use cloud::CloudClient;
pub use ollama::OllamaClient;
pub use prompt::PromptFormat;
pub use router::{LlmConfig, LlmRouter};

#[cfg(feature = "llama")]
pub use llama::LlamaClient;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl CompletionRequest {
    pub fn simple(messages: Vec<Message>) -> Self {
        Self { messages, max_tokens: Some(2048), temperature: 0.7, tools: None }
    }
}

#[derive(Debug, Clone)]
pub enum StreamChunk {
    Token(String),
    Done { total_tokens: u32 },
    Error(String),
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<String>;
    fn provider_name(&self) -> &str;
}
