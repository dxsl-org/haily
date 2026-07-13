//! Wire-level envelope encode/decode. `ServerBody`/`ClientFrame` already carry hand-written
//! `Deserialize` impls in `haily-types` that degrade an unrecognized/malformed tag to
//! `Unknown { type_tag }` instead of erroring (C3) — this module is a thin, crate-local wrapper
//! so `client.rs`/`ws.rs` have one place that turns wire text into typed values (and a single
//! error type for the genuinely-unrecoverable cases: not valid JSON at all, or valid JSON that
//! isn't even an object with a `type` key — see `haily-types`' `Deserialize` impls for why THAT
//! case is a hard error rather than `Unknown`).
use haily_types::{ClientFrame, ServerFrame};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("malformed server frame (not valid envelope JSON): {0}")]
    InvalidJson(#[from] serde_json::Error),
}

/// Decode one `ServerFrame` from wire text. Never returns an error for a frame whose `body`
/// carries an unrecognized `ServerBody` variant or a known variant with a reshaped payload —
/// `haily-types::ServerBody`'s `Deserialize` already turned that into `ServerBody::Unknown`
/// before this function's `serde_json::from_str` call even returns. This only errors when the
/// envelope itself (`epoch`/`seq`/`body` structure) is unparseable — the case
/// `docs/mobile-protocol.md` §9 says SHOULD have bumped `PROTOCOL_VERSION` and therefore
/// hard-blocked at `HelloAck` before any such frame could arrive.
pub fn decode_server_frame(text: &str) -> Result<ServerFrame, DecodeError> {
    Ok(serde_json::from_str(text)?)
}

/// Encode one `ClientFrame` for the wire. Infallible in practice (every field is already a
/// valid serde value) — kept `Result`-returning only so a future non-serializable addition to
/// `ClientFrame` fails loudly at the call site instead of silently panicking deep in `serde_json`.
pub fn encode_client_frame(frame: &ClientFrame) -> Result<String, DecodeError> {
    Ok(serde_json::to_string(frame)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_types::{DepthMode, PROTOCOL_VERSION};
    use uuid::Uuid;

    #[test]
    fn round_trips_a_known_server_frame() {
        let json = r#"{"epoch":7,"seq":3,"body":{"type":"Pong","data":null}}"#;
        let frame = decode_server_frame(json).expect("must decode a known frame");
        assert_eq!(frame.epoch, 7);
        assert_eq!(frame.seq, 3);
    }

    #[test]
    fn an_unrecognized_server_body_variant_decodes_to_unknown_not_an_error() {
        let json = r#"{"epoch":1,"seq":1,"body":{"type":"SomeFutureFrameKind","data":{"x":1}}}"#;
        let frame = decode_server_frame(json).expect("C3: must degrade, not hard-fail");
        assert!(matches!(
            frame.body,
            haily_types::ServerBody::Unknown { .. }
        ));
    }

    #[test]
    fn malformed_envelope_json_is_a_decode_error() {
        let result = decode_server_frame("not json at all");
        assert!(matches!(result, Err(DecodeError::InvalidJson(_))));
    }

    #[test]
    fn encodes_a_client_frame_round_trippable_by_the_server_side_type() {
        let frame = ClientFrame::Hello {
            last_seen_seq: Some(42),
            last_epoch: Some(3),
            protocol_version: PROTOCOL_VERSION,
        };
        let text = encode_client_frame(&frame).expect("encode");
        let round: ClientFrame = serde_json::from_str(&text).expect("round-trip");
        assert!(matches!(
            round,
            ClientFrame::Hello {
                last_seen_seq: Some(42),
                last_epoch: Some(3),
                ..
            }
        ));
    }

    #[test]
    fn encodes_user_message_with_depth() {
        let frame = ClientFrame::UserMessage {
            session_id: Uuid::nil(),
            message: "hi".into(),
            depth: DepthMode::Normal,
        };
        let text = encode_client_frame(&frame).expect("encode");
        assert!(text.contains("\"UserMessage\""));
    }
}
