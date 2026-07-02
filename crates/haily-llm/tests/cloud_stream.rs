//! End-to-end SSE streaming tests against a canned fixture server — proves
//! `CloudClient::complete_stream` parses BOTH dialects (OpenAI, Anthropic) to the
//! same text a non-streaming `complete()` call would have returned, and that a
//! mid-stream disconnect surfaces as `StreamChunk::Error` rather than a silent
//! partial success or a duplicate-triggering retry.
//!
//! Uses a minimal hand-rolled HTTP/1.1 server (raw `TcpListener`) instead of pulling
//! in a mocking crate — the fixture bodies are a handful of canned SSE frames, well
//! within what a ~40-line raw responder can express (YAGNI: a full mock-HTTP crate
//! buys nothing extra here).
use haily_llm::{CompletionRequest, LlmClient, Message, StreamChunk};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Starts a one-shot HTTP server that reads (and discards) one request, then writes
/// `body` verbatim as the response (caller supplies full status line + headers +
/// body). Returns the bound `http://127.0.0.1:<port>` base URL.
async fn spawn_fixture_server(response: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 4096];
        // Best-effort read of the request so the client's `.send()` doesn't hang on
        // a half-open connection; the fixture doesn't need to parse it.
        let _ = socket.read(&mut buf).await;
        socket.write_all(response.as_bytes()).await.expect("write response");
        socket.shutdown().await.ok();
    });

    format!("http://{addr}")
}

fn chunked_sse(events: &[&str]) -> String {
    let body = events.join("");
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
    )
}

async fn collect_tokens(mut rx: tokio::sync::mpsc::Receiver<StreamChunk>) -> (String, bool, bool) {
    let mut text = String::new();
    let mut saw_done = false;
    let mut saw_error = false;
    while let Some(chunk) = rx.recv().await {
        match chunk {
            StreamChunk::Token(t) => text.push_str(&t),
            StreamChunk::Done { .. } => {
                saw_done = true;
                break;
            }
            StreamChunk::Error(_) => {
                saw_error = true;
                break;
            }
        }
    }
    (text, saw_done, saw_error)
}

#[tokio::test]
async fn openai_sse_fixture_streams_to_expected_text() {
    let events = [
        "data: {\"choices\":[{\"delta\":{\"content\":\"Xin \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"chào\"}}]}\n\n",
        "data: [DONE]\n\n",
    ];
    let base_url = spawn_fixture_server(Box::leak(chunked_sse(&events).into_boxed_str())).await;

    let client = haily_llm::CloudClient::new(base_url, vec!["test-key".to_string()], "gpt-4o-mini").unwrap();
    let req = CompletionRequest::simple(vec![Message::user("hi")]);
    let rx = client.complete_stream(req).await.expect("stream must init");

    let (text, saw_done, saw_error) = collect_tokens(rx).await;
    assert_eq!(text, "Xin chào");
    assert!(saw_done, "OpenAI [DONE] must surface as StreamChunk::Done");
    assert!(!saw_error);
}

#[tokio::test]
async fn anthropic_sse_fixture_streams_to_expected_text() {
    let events = [
        "event: message_start\ndata: {}\n\n",
        "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"Xin \"}}\n\n",
        "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"chào\"}}\n\n",
        "event: content_block_stop\ndata: {}\n\n",
        "event: message_stop\ndata: {}\n\n",
    ];
    let base_url = spawn_fixture_server(Box::leak(chunked_sse(&events).into_boxed_str())).await;
    let anthropic_url = format!("{base_url}/anthropic-compatible"); // triggers dialect detection via substring

    let client =
        haily_llm::CloudClient::new(anthropic_url, vec!["test-key".to_string()], "claude-3-5-sonnet").unwrap();
    let req = CompletionRequest::simple(vec![Message::user("hi")]);
    let rx = client.complete_stream(req).await.expect("stream must init");

    let (text, saw_done, saw_error) = collect_tokens(rx).await;
    assert_eq!(text, "Xin chào");
    assert!(saw_done, "Anthropic message_stop must surface as StreamChunk::Done");
    assert!(!saw_error);
}

#[tokio::test]
async fn anthropic_input_json_delta_never_leaks_as_text() {
    // Native tool-calling deltas must be ignored, not forwarded as plain text — this
    // client's protocol is the prompted `<tool_call>` tag, not either provider's
    // structured tool-calling API.
    let events = [
        "event: content_block_delta\ndata: {\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":1\"}}\n\n",
        "event: message_stop\ndata: {}\n\n",
    ];
    let base_url = spawn_fixture_server(Box::leak(chunked_sse(&events).into_boxed_str())).await;
    let anthropic_url = format!("{base_url}/anthropic");

    let client = haily_llm::CloudClient::new(anthropic_url, vec!["test-key".to_string()], "claude-3-5-sonnet").unwrap();
    let req = CompletionRequest::simple(vec![Message::user("hi")]);
    let rx = client.complete_stream(req).await.expect("stream must init");

    let (text, saw_done, _) = collect_tokens(rx).await;
    assert_eq!(text, "", "input_json_delta fragments must never appear as user-visible text");
    assert!(saw_done);
}

#[tokio::test]
async fn mid_stream_disconnect_surfaces_as_error_not_silent_success() {
    // Server sends one token then closes the connection without [DONE] — simulates
    // a network drop mid-stream. Must surface as StreamChunk::Error, never a clean
    // Done and never a silent retry (which would duplicate the partial text).
    let events = ["data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"];
    let base_url = spawn_fixture_server(Box::leak(chunked_sse(&events).into_boxed_str())).await;

    let client = haily_llm::CloudClient::new(base_url, vec!["test-key".to_string()], "gpt-4o-mini").unwrap();
    let req = CompletionRequest::simple(vec![Message::user("hi")]);
    let rx = client.complete_stream(req).await.expect("stream must init");

    let (text, saw_done, saw_error) = collect_tokens(rx).await;
    assert_eq!(text, "partial", "text streamed before the disconnect must still be delivered");
    assert!(saw_error, "an unexpected disconnect must surface as StreamChunk::Error");
    assert!(!saw_done, "a disconnect must never be reported as a clean Done");
}

#[tokio::test]
async fn anthropic_error_event_surfaces_as_stream_error() {
    let events = [
        "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        "event: error\ndata: {\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    ];
    let base_url = spawn_fixture_server(Box::leak(chunked_sse(&events).into_boxed_str())).await;
    let anthropic_url = format!("{base_url}/anthropic");

    let client = haily_llm::CloudClient::new(anthropic_url, vec!["test-key".to_string()], "claude-3-5-sonnet").unwrap();
    let req = CompletionRequest::simple(vec![Message::user("hi")]);
    let rx = client.complete_stream(req).await.expect("stream must init");

    let (text, saw_done, saw_error) = collect_tokens(rx).await;
    assert_eq!(text, "partial");
    assert!(saw_error);
    assert!(!saw_done);
}
