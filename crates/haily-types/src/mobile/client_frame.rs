//! `ClientFrame` — every frame the mobile client can send over the WS connection (post
//! handshake — pairing is a separate HTTP contract, see `pairing.rs`). See `mobile/mod.rs`'s
//! module doc for why `Deserialize` is hand-written instead of derived.

use crate::DepthMode;
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

/// Every payload the client can send. The device token authenticates the WS upgrade itself
/// (`Authorization` header) — NOT any individual frame; every SESSION-scoped frame below
/// additionally carries `session_id` so the server can enforce `session_id ∈` this
/// authenticated device's sessions on each one (red team m1), not just on `Approve`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum ClientFrame {
    /// First frame after connect. `last_seen_seq`/`last_epoch` are `None` on a fresh
    /// pairing/first connect; set from the client's stored cursor on reconnect so the server
    /// can decide replay-from-cursor vs full resync vs `ResumeWindowExceeded` (see
    /// `docs/mobile-protocol.md` § Resume Semantics).
    Hello {
        last_seen_seq: Option<u64>,
        last_epoch: Option<u64>,
        protocol_version: u16,
    },
    /// A chat turn. `depth` defaults to `Normal` (`DepthMode`'s own default) so an older
    /// client that predates depth selection still sends a decodable frame.
    UserMessage {
        session_id: Uuid,
        message: String,
        #[serde(default)]
        depth: DepthMode,
    },
    /// Resolves a pending `ResponseChunk::ToolApprovalRequest`. `biometric_ok` reports
    /// whether the phone's OWN OS-level biometric/passcode prompt succeeded BEFORE this frame
    /// was sent — the server, not the client, decides whether that was REQUIRED for this
    /// approval's risk tier (`MobileApprovalPolicy`); a client that skips the prompt simply
    /// sends `false` and the server enforces its policy (red team M1).
    Approve {
        approval_id: Uuid,
        session_id: Uuid,
        approved: bool,
        biometric_ok: bool,
    },
    /// Toggles the (global, red team M15) kill switch. ENABLE-ONLY when remote: the server
    /// MUST reject `on: false` from a mobile connection — disabling requires the desktop
    /// (red team M1). Still session-scoped per m1 so the request is bound to an
    /// authenticated device/session pair.
    SetKillSwitch { session_id: Uuid, on: bool },
    /// Requests the current proactive-card list.
    FetchProactive { session_id: Uuid },
    /// Requests a [`super::SessionSnapshot`] — the recovery path when resume-by-seq is no
    /// longer possible (red team M7).
    FetchSession { session_id: Uuid },
    /// Keepalive; the server replies `ServerBody::Pong`.
    Ping,
    /// Forward-compat sentinel (red team C3), symmetric with `ServerBody::Unknown` — see that
    /// type's doc for why this is hand-decoded rather than `#[serde(other)]`, and for the
    /// distinction between an unrecognized `type_tag` (benign — the older-server-vs-newer-
    /// client direction) versus a KNOWN `type_tag` reaching `Unknown` (a recognized frame kind
    /// whose payload was corrupt or shaped incompatibly — worth a different server-side
    /// log/alert treatment; P2a concern, this type only carries the tag).
    ///
    /// Never constructed by client-side encode logic — only by this type's `Deserialize` impl
    /// below. A server receiving this from a client should ignore/log it rather than
    /// disconnect the socket.
    ///
    /// The fallback mechanism is `serde_json`-coupled (see [`Deserialize`] below) — it does
    /// not generalize to a non-JSON wire transport without its own bridging value type.
    Unknown { type_tag: String },
}

/// Mirrors [`ClientFrame`] minus `Unknown` — see `ServerBody`'s equivalent shadow enum
/// (`server_body.rs`) for why this pattern exists (private to this file).
#[derive(Deserialize)]
#[serde(tag = "type", content = "data")]
enum ClientFrameKnown {
    Hello {
        last_seen_seq: Option<u64>,
        last_epoch: Option<u64>,
        protocol_version: u16,
    },
    UserMessage {
        session_id: Uuid,
        message: String,
        #[serde(default)]
        depth: DepthMode,
    },
    Approve {
        approval_id: Uuid,
        session_id: Uuid,
        approved: bool,
        biometric_ok: bool,
    },
    SetKillSwitch {
        session_id: Uuid,
        on: bool,
    },
    FetchProactive {
        session_id: Uuid,
    },
    FetchSession {
        session_id: Uuid,
    },
    Ping,
}

impl From<ClientFrameKnown> for ClientFrame {
    fn from(known: ClientFrameKnown) -> Self {
        match known {
            ClientFrameKnown::Hello {
                last_seen_seq,
                last_epoch,
                protocol_version,
            } => ClientFrame::Hello {
                last_seen_seq,
                last_epoch,
                protocol_version,
            },
            ClientFrameKnown::UserMessage {
                session_id,
                message,
                depth,
            } => ClientFrame::UserMessage {
                session_id,
                message,
                depth,
            },
            ClientFrameKnown::Approve {
                approval_id,
                session_id,
                approved,
                biometric_ok,
            } => ClientFrame::Approve {
                approval_id,
                session_id,
                approved,
                biometric_ok,
            },
            ClientFrameKnown::SetKillSwitch { session_id, on } => {
                ClientFrame::SetKillSwitch { session_id, on }
            }
            ClientFrameKnown::FetchProactive { session_id } => {
                ClientFrame::FetchProactive { session_id }
            }
            ClientFrameKnown::FetchSession { session_id } => {
                ClientFrame::FetchSession { session_id }
            }
            ClientFrameKnown::Ping => ClientFrame::Ping,
        }
    }
}

impl<'de> Deserialize<'de> for ClientFrame {
    /// See `ServerBody`'s `Deserialize` impl (`server_body.rs`) — identical strategy,
    /// mirrored for the opposite direction. Only the `type` tag string is ever cloned
    /// (extracted up front, before `value` is moved into the typed decode attempt) — the
    /// hot, successful-decode path pays no deep clone of the frame body.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let type_tag = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        match serde_json::from_value::<ClientFrameKnown>(value) {
            Ok(known) => Ok(known.into()),
            Err(_) => type_tag
                .map(|type_tag| ClientFrame::Unknown { type_tag })
                .ok_or_else(|| DeError::custom("ClientFrame: missing or non-string \"type\" tag")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors `ServerBody`'s equivalent test — a server receiving a newer-than-known client
    /// frame must not disconnect the socket.
    #[test]
    fn unknown_future_variant_decodes_gracefully() {
        let json = r#"{"type":"SomeFutureClientAction","data":{"x":1}}"#;
        let frame: ClientFrame = serde_json::from_str(json).expect("must not hard-fail");
        match frame {
            ClientFrame::Unknown { type_tag } => assert_eq!(type_tag, "SomeFutureClientAction"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    /// The REAL C3 scenario, not just a synthetic top-level tag: the OUTER frame type
    /// (`"UserMessage"`) is perfectly recognized, but a field's shape changed incompatibly
    /// (here: `depth` is not a value `DepthMode` can parse). The whole frame must degrade to
    /// `Unknown`, tagged with the OUTER type — never accept a partially-decoded frame.
    #[test]
    fn known_outer_tag_with_incompatible_inner_field_degrades_to_unknown_tagged_with_outer_type() {
        let json = r#"{"type":"UserMessage","data":{"session_id":"00000000-0000-0000-0000-000000000000","message":"hi","depth":{"nested":"future-shape"}}}"#;
        let frame: ClientFrame = serde_json::from_str(json).expect("must degrade, not hard-fail");
        match frame {
            ClientFrame::Unknown { type_tag } => assert_eq!(type_tag, "UserMessage"),
            other => panic!("expected Unknown{{type_tag:\"UserMessage\"}}, got {other:?}"),
        }
    }

    #[test]
    fn missing_type_tag_is_an_error_not_a_panic() {
        let json = r#"{"data":{"x":1}}"#;
        let result: Result<ClientFrame, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "missing type tag must error, not decode to a variant"
        );
    }

    #[test]
    fn null_data_on_a_known_tag_degrades_to_unknown() {
        let json = r#"{"type":"UserMessage","data":null}"#;
        let frame: ClientFrame = serde_json::from_str(json).expect("must degrade, not hard-fail");
        match frame {
            ClientFrame::Unknown { type_tag } => assert_eq!(type_tag, "UserMessage"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn malformed_data_shape_on_a_known_tag_degrades_to_unknown() {
        let json = r#"{"type":"UserMessage","data":[1,2,3]}"#;
        let frame: ClientFrame = serde_json::from_str(json).expect("must degrade, not hard-fail");
        match frame {
            ClientFrame::Unknown { type_tag } => assert_eq!(type_tag, "UserMessage"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn non_string_type_tag_is_an_error_not_a_panic() {
        let json = r#"{"type":123,"data":{}}"#;
        let result: Result<ClientFrame, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "a non-string type tag must error, not panic or silently decode"
        );
    }

    /// m1: every session-scoped `ClientFrame` variant carries `session_id` on the wire, not
    /// just `Approve` — asserted by grepping the serialized `data` object for the key.
    #[test]
    fn every_session_scoped_variant_carries_session_id() {
        let sid = Uuid::new_v4();
        let frames = vec![
            ClientFrame::UserMessage {
                session_id: sid,
                message: "hi".into(),
                depth: DepthMode::Normal,
            },
            ClientFrame::Approve {
                approval_id: Uuid::new_v4(),
                session_id: sid,
                approved: true,
                biometric_ok: true,
            },
            ClientFrame::SetKillSwitch {
                session_id: sid,
                on: true,
            },
            ClientFrame::FetchProactive { session_id: sid },
            ClientFrame::FetchSession { session_id: sid },
        ];
        for frame in frames {
            let json = serde_json::to_string(&frame).expect("serialize");
            assert!(
                json.contains("\"session_id\""),
                "missing session_id in {json}"
            );
        }
    }

    #[test]
    fn hello_carries_optional_resume_cursor() {
        let fresh = ClientFrame::Hello {
            last_seen_seq: None,
            last_epoch: None,
            protocol_version: crate::PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&fresh).expect("serialize");
        let round: ClientFrame = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            round,
            ClientFrame::Hello {
                last_seen_seq: None,
                last_epoch: None,
                ..
            }
        ));

        let resuming = ClientFrame::Hello {
            last_seen_seq: Some(99),
            last_epoch: Some(5),
            protocol_version: crate::PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&resuming).expect("serialize");
        let round: ClientFrame = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            round,
            ClientFrame::Hello {
                last_seen_seq: Some(99),
                last_epoch: Some(5),
                ..
            }
        ));
    }
}
