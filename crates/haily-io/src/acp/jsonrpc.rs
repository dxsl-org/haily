//! Newline-delimited JSON-RPC 2.0 framing for the ACP channel (Sub-Agent + Skill
//! Architecture phase 12).
//!
//! This is the hand-rolled dialect the plan documents as the fallback when the Zed
//! `agent-client-protocol` crate does not fit (see the phase Deviation Log). It is a
//! minimal, bidirectional JSON-RPC 2.0 codec: each message is exactly one line of JSON
//! terminated by `\n`. The transport is bidirectional because an ACP agent both ANSWERS
//! client requests (`session/*`) AND ISSUES requests to the client (`session/request_permission`).
//!
//! Pure functions only — every frame the adapter can emit is built here, so the
//! "stdout carries ONLY valid JSON-RPC frames" discipline is provable by unit-testing that
//! each builder produces a single valid JSON object with `"jsonrpc":"2.0"` and no embedded
//! newline. All non-frame output (logs) MUST go to stderr; the `run_acp` entry point
//! configures tracing accordingly.

use serde::Serialize;
use serde_json::{json, Value};

/// The only JSON-RPC version this dialect speaks.
pub const JSONRPC_VERSION: &str = "2.0";

/// Standard JSON-RPC error codes used by the ACP handlers.
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// A parsed inbound line. `Invalid` captures a non-JSON or malformed line so the read loop
/// can log-and-skip it rather than crashing the always-on transport (a corrupt line from a
/// misbehaving client must never take the channel down).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// A client request expecting a response (`id` present + `method`).
    Request { id: Value, method: String, params: Value },
    /// A client notification (no `id`) — fire-and-forget.
    Notification { method: String, params: Value },
    /// A response to a request THIS agent previously issued (e.g. `request_permission`).
    Response { id: Value, result: Option<Value>, error: Option<Value> },
    /// Unparseable / not a JSON-RPC object. Carries the reason for logging.
    Invalid(String),
}

/// Parse one inbound line into an [`Incoming`]. Never panics: a malformed line becomes
/// [`Incoming::Invalid`]. The discriminator is structural — a `method` present means it is
/// inbound (request or notification); otherwise it is a response to one of our requests.
pub fn parse_incoming(line: &str) -> Incoming {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Incoming::Invalid("empty line".to_string());
    }
    let v: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => return Incoming::Invalid(format!("not JSON: {e}")),
    };
    if !v.is_object() {
        return Incoming::Invalid("frame is not a JSON object".to_string());
    }
    let id = v.get("id").cloned();
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    match v.get("method").and_then(Value::as_str) {
        Some(method) => {
            let method = method.to_string();
            match id {
                Some(id) if !id.is_null() => Incoming::Request { id, method, params },
                _ => Incoming::Notification { method, params },
            }
        }
        // No method → it is a response to a request we issued.
        None => match id {
            Some(id) if !id.is_null() => Incoming::Response {
                id,
                result: v.get("result").cloned(),
                error: v.get("error").cloned(),
            },
            _ => Incoming::Invalid("frame has neither method nor id".to_string()),
        },
    }
}

/// Serialize `payload` to a single-line frame terminated by `\n`. Serialization of a
/// concrete DTO cannot fail in practice, but a fallback keeps this total (never panics)
/// and never emits a partial/embedded-newline frame.
fn frame<T: Serialize>(payload: &T) -> String {
    match serde_json::to_string(payload) {
        Ok(mut s) => {
            // serde_json never emits a raw newline, but guarantee the invariant anyway so a
            // future struct change can never smuggle one and split the stream.
            s.retain(|c| c != '\n' && c != '\r');
            s.push('\n');
            s
        }
        Err(_) => "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"encode failed\"}}\n".to_string(),
    }
}

/// A request frame FROM this agent to the client (e.g. `session/request_permission`).
pub fn request_frame(id: &Value, method: &str, params: Value) -> String {
    frame(&json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "method": method, "params": params }))
}

/// A notification frame (no id) FROM this agent to the client (e.g. `session/update`).
pub fn notification_frame(method: &str, params: Value) -> String {
    frame(&json!({ "jsonrpc": JSONRPC_VERSION, "method": method, "params": params }))
}

/// A success response to a client request.
pub fn response_ok_frame(id: &Value, result: Value) -> String {
    frame(&json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "result": result }))
}

/// An error response to a client request.
pub fn response_err_frame(id: &Value, code: i64, message: &str) -> String {
    frame(&json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "error": { "code": code, "message": message } }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every builder must produce exactly one line (single trailing `\n`, no embedded
    /// newline) that parses as a JSON object carrying `"jsonrpc":"2.0"` — this is the
    /// machine-checkable core of the "stdout carries ONLY protocol frames" discipline.
    fn assert_is_one_valid_frame(line: &str) {
        assert!(line.ends_with('\n'), "frame must be newline-terminated: {line:?}");
        assert_eq!(line.matches('\n').count(), 1, "frame must be exactly one line: {line:?}");
        let body = line.trim_end();
        assert!(!body.contains('\n') && !body.contains('\r'), "no embedded newline");
        let v: Value = serde_json::from_str(body).expect("frame must be valid JSON");
        assert_eq!(v["jsonrpc"], "2.0", "every frame carries jsonrpc 2.0");
    }

    #[test]
    fn all_outbound_builders_emit_a_single_valid_jsonrpc_frame() {
        assert_is_one_valid_frame(&request_frame(&json!(1), "session/request_permission", json!({"x":1})));
        assert_is_one_valid_frame(&notification_frame("session/update", json!({"sessionId":"s"})));
        assert_is_one_valid_frame(&response_ok_frame(&json!("abc"), json!({"ok":true})));
        assert_is_one_valid_frame(&response_err_frame(&json!(2), METHOD_NOT_FOUND, "nope"));
    }

    #[test]
    fn embedded_newlines_in_content_cannot_split_a_frame() {
        // A crafted multi-line string in params must still yield ONE line on the wire.
        let line = notification_frame("session/update", json!({"text":"a\nb\r\nc"}));
        assert_eq!(line.matches('\n').count(), 1, "content newlines must be escaped/stripped, not split the frame");
        assert_is_one_valid_frame(&line);
    }

    #[test]
    fn parse_distinguishes_request_notification_and_response() {
        match parse_incoming(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{}}"#) {
            Incoming::Request { method, .. } => assert_eq!(method, "session/new"),
            other => panic!("expected request, got {other:?}"),
        }
        match parse_incoming(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#) {
            Incoming::Notification { method, .. } => assert_eq!(method, "session/cancel"),
            other => panic!("expected notification, got {other:?}"),
        }
        match parse_incoming(r#"{"jsonrpc":"2.0","id":7,"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}"#) {
            Incoming::Response { id, result, error } => {
                assert_eq!(id, json!(7));
                assert!(result.is_some() && error.is_none());
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[test]
    fn malformed_lines_become_invalid_never_panic() {
        assert!(matches!(parse_incoming("not json"), Incoming::Invalid(_)));
        assert!(matches!(parse_incoming(""), Incoming::Invalid(_)));
        assert!(matches!(parse_incoming("[1,2,3]"), Incoming::Invalid(_)));
        assert!(matches!(parse_incoming(r#"{"jsonrpc":"2.0"}"#), Incoming::Invalid(_)));
    }
}
