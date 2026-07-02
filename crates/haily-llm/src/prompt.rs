use crate::{Message, Role};

/// Prompt format selector for embedded GGUF inference.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum PromptFormat {
    /// ChatML — used by Qwen2.5 and most modern instruction-tuned GGUF models.
    #[default]
    ChatML,
    /// Gemma 4 turn format — `<start_of_turn>user\n...<end_of_turn>\n<start_of_turn>model\n`.
    /// System prompt is prepended to the first user message.
    Gemma4,
}

impl PromptFormat {
    pub fn format(self, messages: &[Message]) -> String {
        match self {
            PromptFormat::ChatML => chatml(messages),
            PromptFormat::Gemma4 => gemma4(messages),
        }
    }

    /// Parse a prompt format from its config string. Infallible — unknown
    /// values fall back to `PromptFormat::ChatML`, so this deliberately does not
    /// implement `std::str::FromStr` (which would force a meaningless error type).
    pub fn from_name(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "gemma4" | "gemma" => PromptFormat::Gemma4,
            _ => PromptFormat::ChatML,
        }
    }
}

/// Formats messages in ChatML format, used by Qwen2.5 and most modern GGUF models.
/// The assistant turn is left open for the model to complete.
pub fn chatml(messages: &[Message]) -> String {
    let mut out = String::with_capacity(512);
    for msg in messages {
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        out.push_str(&format!("<|im_start|>{}\n{}\n<|im_end|>\n", role, msg.content));
    }
    out.push_str("<|im_start|>assistant\n");
    out
}

/// Formats messages in Gemma 4 turn format.
/// System content is prepended to the first user turn (Gemma has no dedicated system role).
pub fn gemma4(messages: &[Message]) -> String {
    let mut out = String::with_capacity(512);
    // Collect system prompt(s) to prepend to first user turn.
    let system_text: String = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut system_injected = system_text.is_empty();

    for msg in messages {
        match msg.role {
            Role::System => continue,
            Role::User => {
                let content = if !system_injected {
                    system_injected = true;
                    format!("{}\n\n{}", system_text, msg.content)
                } else {
                    msg.content.clone()
                };
                out.push_str(&format!("<start_of_turn>user\n{content}<end_of_turn>\n"));
            }
            Role::Assistant | Role::Tool => {
                out.push_str(&format!(
                    "<start_of_turn>model\n{}<end_of_turn>\n",
                    msg.content
                ));
            }
        }
    }
    out.push_str("<start_of_turn>model\n");
    out
}

/// Formats messages for the Ollama /api/chat JSON body.
pub fn to_ollama_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            serde_json::json!({ "role": role, "content": m.content })
        })
        .collect()
}

/// Formats messages for the OpenAI /v1/chat/completions JSON body.
pub fn to_openai_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    to_ollama_messages(messages) // same schema
}
