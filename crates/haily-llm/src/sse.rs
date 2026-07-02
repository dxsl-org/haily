//! SSE dialect parsing for cloud streaming — OpenAI (`data:` lines, JSON delta
//! payload, literal `[DONE]` terminator) and Anthropic (named events: `message_start`,
//! `content_block_delta`, `message_stop`, `error`, ...). Both dialects are parsed on
//! top of `eventsource_stream::Event` (the low-level SSE frame), which is dialect-
//! agnostic — only the frame's `data` JSON shape differs between them.
//!
//! Kept separate from `cloud.rs` so the request-building/key-rotation logic (which
//! doesn't change with streaming) stays uncluttered by parsing detail.
use eventsource_stream::Event;
use serde::Deserialize;

/// Which SSE dialect a cloud endpoint speaks. Inferred from `base_url` — there is no
/// explicit config field for this today (the non-streaming `complete()` path predates
/// any Anthropic support entirely and is out of scope for this phase); a substring
/// check on the configured base URL is the minimal addition that lets streaming
/// support both without a wider config-schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    OpenAi,
    Anthropic,
}

impl Dialect {
    pub fn from_base_url(base_url: &str) -> Self {
        if base_url.to_ascii_lowercase().contains("anthropic") {
            Dialect::Anthropic
        } else {
            Dialect::OpenAi
        }
    }
}

/// Result of interpreting one SSE frame under a given dialect.
pub enum ParsedEvent {
    /// A text delta to forward to the user as a `StreamChunk::Token`.
    Delta(String),
    /// Clean end-of-stream signal (OpenAI's `[DONE]`, Anthropic's `message_stop`).
    Done,
    /// An in-band error event (Anthropic's `error` event, or a JSON parse failure
    /// severe enough that the stream cannot continue meaningfully).
    Error(String),
    /// Frame carries no user-visible content (Anthropic's `ping`,
    /// `content_block_start`/`stop`, `message_delta`, etc.) — skip and keep reading.
    Ignore,
}

#[derive(Deserialize)]
struct OpenAiChunk {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize, Default)]
struct OpenAiChoice {
    delta: OpenAiDelta,
}

#[derive(Deserialize, Default)]
struct OpenAiDelta {
    content: Option<String>,
}

/// Parses one SSE frame under the OpenAI dialect: `data: {json}` per event, with a
/// literal `data: [DONE]` sentinel terminating the stream — `[DONE]` is a plain
/// string payload, not a standard SSE close, so it must be special-cased before JSON
/// parsing is attempted.
pub fn parse_openai_event(event: &Event) -> ParsedEvent {
    let data = event.data.trim();
    if data == "[DONE]" {
        return ParsedEvent::Done;
    }
    if data.is_empty() {
        return ParsedEvent::Ignore;
    }
    match serde_json::from_str::<OpenAiChunk>(data) {
        Ok(chunk) => match chunk.choices.into_iter().next().and_then(|c| c.delta.content) {
            Some(text) if !text.is_empty() => ParsedEvent::Delta(text),
            _ => ParsedEvent::Ignore,
        },
        Err(e) => ParsedEvent::Error(format!("malformed OpenAI SSE payload: {e}")),
    }
}

#[derive(Deserialize)]
struct AnthropicDeltaPayload {
    delta: Option<AnthropicDelta>,
}

#[derive(Deserialize)]
struct AnthropicDelta {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicErrorPayload {
    error: Option<AnthropicErrorDetail>,
}

#[derive(Deserialize)]
struct AnthropicErrorDetail {
    message: Option<String>,
}

/// Parses one SSE frame under the Anthropic dialect: named `event:` field selects the
/// interpretation, not the `data:` payload shape alone. Only `content_block_delta`
/// frames with `delta.type == "text_delta"` carry user-visible text — `input_json_delta`
/// (tool-call argument fragments) is intentionally NOT surfaced as a `Delta` here: this
/// client's tool-calling protocol is the prompted `<tool_call>` text tag (see
/// `tool_call.rs`), not either provider's native structured tool-calling API, so a
/// native `input_json_delta` has nothing to buffer into today and is ignored rather
/// than mis-forwarded as plain text.
pub fn parse_anthropic_event(event: &Event) -> ParsedEvent {
    match event.event.as_str() {
        "content_block_delta" => match serde_json::from_str::<AnthropicDeltaPayload>(&event.data) {
            Ok(payload) => match payload.delta {
                Some(d) if d.kind == "text_delta" => match d.text {
                    Some(text) if !text.is_empty() => ParsedEvent::Delta(text),
                    _ => ParsedEvent::Ignore,
                },
                _ => ParsedEvent::Ignore, // input_json_delta or unknown delta kind
            },
            Err(e) => ParsedEvent::Error(format!("malformed Anthropic content_block_delta: {e}")),
        },
        "message_stop" => ParsedEvent::Done,
        "error" => {
            let msg = serde_json::from_str::<AnthropicErrorPayload>(&event.data)
                .ok()
                .and_then(|p| p.error)
                .and_then(|e| e.message)
                .unwrap_or_else(|| "unknown Anthropic stream error".to_string());
            ParsedEvent::Error(msg)
        }
        // message_start, content_block_start/stop, message_delta, ping, and any
        // future event name — none carry text this client needs mid-stream.
        _ => ParsedEvent::Ignore,
    }
}

pub fn parse_event(dialect: Dialect, event: &Event) -> ParsedEvent {
    match dialect {
        Dialect::OpenAi => parse_openai_event(event),
        Dialect::Anthropic => parse_anthropic_event(event),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai_event(data: &str) -> Event {
        Event { event: String::new(), data: data.to_string(), id: String::new(), retry: None }
    }

    fn anthropic_event(event_name: &str, data: &str) -> Event {
        Event { event: event_name.to_string(), data: data.to_string(), id: String::new(), retry: None }
    }

    #[test]
    fn dialect_detects_anthropic_from_base_url() {
        assert_eq!(Dialect::from_base_url("https://api.anthropic.com"), Dialect::Anthropic);
        assert_eq!(Dialect::from_base_url("https://api.openai.com"), Dialect::OpenAi);
    }

    #[test]
    fn openai_parses_text_delta() {
        let e = openai_event(r#"{"choices":[{"delta":{"content":"hello"}}]}"#);
        match parse_openai_event(&e) {
            ParsedEvent::Delta(text) => assert_eq!(text, "hello"),
            _ => panic!("expected Delta"),
        }
    }

    #[test]
    fn openai_done_sentinel_ends_stream() {
        assert!(matches!(parse_openai_event(&openai_event("[DONE]")), ParsedEvent::Done));
    }

    #[test]
    fn openai_ignores_empty_delta() {
        let e = openai_event(r#"{"choices":[{"delta":{}}]}"#);
        assert!(matches!(parse_openai_event(&e), ParsedEvent::Ignore));
    }

    #[test]
    fn openai_malformed_json_surfaces_as_error_not_panic() {
        let e = openai_event("{not json");
        assert!(matches!(parse_openai_event(&e), ParsedEvent::Error(_)));
    }

    #[test]
    fn anthropic_parses_text_delta() {
        let e = anthropic_event("content_block_delta", r#"{"delta":{"type":"text_delta","text":"hi"}}"#);
        match parse_anthropic_event(&e) {
            ParsedEvent::Delta(text) => assert_eq!(text, "hi"),
            _ => panic!("expected Delta"),
        }
    }

    #[test]
    fn anthropic_ignores_input_json_delta() {
        // Tool-call argument fragments under Anthropic's native tool-calling API —
        // this client doesn't use that API (prompted `<tool_call>` tag instead), so
        // these must never leak as plain text.
        let e = anthropic_event(
            "content_block_delta",
            r#"{"delta":{"type":"input_json_delta","partial_json":"{\"a\":1"}}"#,
        );
        assert!(matches!(parse_anthropic_event(&e), ParsedEvent::Ignore));
    }

    #[test]
    fn anthropic_message_stop_ends_stream() {
        let e = anthropic_event("message_stop", "{}");
        assert!(matches!(parse_anthropic_event(&e), ParsedEvent::Done));
    }

    #[test]
    fn anthropic_error_event_surfaces_message() {
        let e = anthropic_event("error", r#"{"error":{"type":"overloaded_error","message":"Overloaded"}}"#);
        match parse_anthropic_event(&e) {
            ParsedEvent::Error(msg) => assert_eq!(msg, "Overloaded"),
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn anthropic_ignores_ping_and_message_start() {
        assert!(matches!(parse_anthropic_event(&anthropic_event("ping", "{}")), ParsedEvent::Ignore));
        assert!(matches!(
            parse_anthropic_event(&anthropic_event("message_start", "{}")),
            ParsedEvent::Ignore
        ));
    }
}
