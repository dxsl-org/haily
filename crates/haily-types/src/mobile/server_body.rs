//! `ServerBody` — the payload half of [`super::ServerFrame`]. See `mobile/mod.rs`'s module
//! doc for why `Deserialize` is hand-written instead of derived.

use super::{MobileApprovalPolicy, MobileError, SessionSnapshot};
use crate::{Notification, ProactiveCard, ResponseChunk, RunEvent};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

/// Every payload the server can send inside a [`super::ServerFrame`]. Reuses the existing
/// `haily-types` shapes (`ResponseChunk`, `RunEvent`, `Notification`, `ProactiveCard`) as
/// envelope PAYLOADS rather than re-deriving parallel mobile-only versions, so mobile renders
/// identically to the GUI for the same underlying event.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum ServerBody {
    /// Handshake reply (see `docs/mobile-protocol.md` § Handshake). `epoch` lets the client
    /// detect a server restart; `protocol_version` is this boot's negotiated version;
    /// `kill_on`/`mobile_approval_policy` seed the client's initial UI state without a
    /// separate round-trip.
    HelloAck {
        epoch: u64,
        protocol_version: u16,
        kill_on: bool,
        mobile_approval_policy: MobileApprovalPolicy,
    },
    /// One `ResponseChunk` from an active turn, scoped to `session_id` (m1: every
    /// session-scoped frame carries it so the server can enforce `session_id ∈` this
    /// device's sessions).
    Chunk {
        session_id: Uuid,
        chunk: ResponseChunk,
    },
    /// One pipeline `RunEvent`, scoped to `session_id`.
    Run { session_id: Uuid, event: RunEvent },
    /// A daemon-wide notification (morning brief, alert, reminder, distillation proposal).
    Notify(Notification),
    /// Full proactive-card list, the reply to `ClientFrame::FetchProactive`.
    ProactiveList(Vec<ProactiveCard>),
    /// The reply to `ClientFrame::FetchSession` (red team M7).
    SessionSnapshot(SessionSnapshot),
    /// Kill-switch state changed (red team m7: broadcast to every frontend, kill switch is
    /// intentionally GLOBAL — M15).
    KillState { on: bool },
    /// A request/pairing-adjacent error surfaced post-connect (see [`MobileError`]).
    Error(MobileError),
    /// Reply to `ClientFrame::Ping` — keepalive, also advances the seq cursor (see
    /// `ServerFrame` doc).
    Pong,
    /// Forward-compat sentinel (red team C3). Produced ONLY by [`Deserialize`] when the wire
    /// `type` tag matches none of the variants above, OR a RECOGNIZED tag's `data` fails to
    /// parse into that variant's shape. These are two different failure modes sharing one
    /// wire representation: an unrecognized `type_tag` is the expected, benign forward-compat
    /// case (an old client meeting a newer server's new frame kind); a KNOWN `type_tag` (e.g.
    /// `"Chunk"`) reaching `Unknown` means the payload for an ALREADY-understood frame kind
    /// was corrupt or changed shape incompatibly — a client should log/alert on the latter
    /// differently than the former (P3 concern; this type only carries the tag, it does not
    /// itself distinguish the two cases).
    ///
    /// Never constructed by server-side encode logic — only by this type's `Deserialize` impl
    /// below. (P2a: a `debug_assert!` at the encode call site that the server never
    /// intentionally emits `Unknown` would catch a regression of this invariant.)
    ///
    /// `type_tag` is kept for logging/telemetry only; the associated `data` is intentionally
    /// discarded — a client renders this as an inert "unsupported event" placeholder, never
    /// interpreted.
    ///
    /// The fallback mechanism is `serde_json`-coupled (see [`Deserialize`] below, which
    /// buffers the frame as a `serde_json::Value` before attempting the typed decode) — it
    /// does not generalize to a non-JSON wire transport without its own bridging value type.
    Unknown { type_tag: String },
}

/// Mirrors [`ServerBody`] minus `Unknown`, with an ordinary derived `Deserialize`. Used ONLY
/// as the "does this JSON match a KNOWN variant" probe inside [`ServerBody`]'s hand-written
/// `Deserialize` — kept private so nothing outside this file can observe the split.
#[derive(Deserialize)]
#[serde(tag = "type", content = "data")]
enum ServerBodyKnown {
    HelloAck {
        epoch: u64,
        protocol_version: u16,
        kill_on: bool,
        mobile_approval_policy: MobileApprovalPolicy,
    },
    Chunk {
        session_id: Uuid,
        chunk: ResponseChunk,
    },
    Run {
        session_id: Uuid,
        event: RunEvent,
    },
    Notify(Notification),
    ProactiveList(Vec<ProactiveCard>),
    SessionSnapshot(SessionSnapshot),
    KillState {
        on: bool,
    },
    Error(MobileError),
    Pong,
}

impl From<ServerBodyKnown> for ServerBody {
    fn from(known: ServerBodyKnown) -> Self {
        match known {
            ServerBodyKnown::HelloAck {
                epoch,
                protocol_version,
                kill_on,
                mobile_approval_policy,
            } => ServerBody::HelloAck {
                epoch,
                protocol_version,
                kill_on,
                mobile_approval_policy,
            },
            ServerBodyKnown::Chunk { session_id, chunk } => ServerBody::Chunk { session_id, chunk },
            ServerBodyKnown::Run { session_id, event } => ServerBody::Run { session_id, event },
            ServerBodyKnown::Notify(n) => ServerBody::Notify(n),
            ServerBodyKnown::ProactiveList(l) => ServerBody::ProactiveList(l),
            ServerBodyKnown::SessionSnapshot(s) => ServerBody::SessionSnapshot(s),
            ServerBodyKnown::KillState { on } => ServerBody::KillState { on },
            ServerBodyKnown::Error(e) => ServerBody::Error(e),
            ServerBodyKnown::Pong => ServerBody::Pong,
        }
    }
}

impl<'de> Deserialize<'de> for ServerBody {
    /// Buffers the whole frame body as a generic JSON value first (the forward-compat seam),
    /// then tries the exhaustive [`ServerBodyKnown`] match. A parse failure — unrecognized
    /// `type` tag OR a known tag whose `data` shape changed incompatibly — degrades to
    /// `Unknown` rather than propagating a hard decode error, satisfying C3's "old client
    /// tolerates a new server variant" guarantee.
    ///
    /// Only the `type` tag string is ever cloned (extracted up front, before `value` is moved
    /// into the typed decode attempt) — the hot, successful-decode path pays no deep clone of
    /// the frame body.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let type_tag = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        match serde_json::from_value::<ServerBodyKnown>(value) {
            Ok(known) => Ok(known.into()),
            Err(_) => type_tag
                .map(|type_tag| ServerBody::Unknown { type_tag })
                .ok_or_else(|| DeError::custom("ServerBody: missing or non-string \"type\" tag")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProactiveCardKind;

    /// The C3 guarantee: a NEW server variant this crate's version doesn't know about decodes
    /// to `Unknown` (carrying the tag for logging) instead of hard-failing the whole frame.
    #[test]
    fn unknown_future_variant_decodes_gracefully() {
        let json = r#"{"type":"SomeFutureFrameKind","data":{"totally":"new","shape":1}}"#;
        let body: ServerBody = serde_json::from_str(json).expect("must not hard-fail");
        match body {
            ServerBody::Unknown { type_tag } => assert_eq!(type_tag, "SomeFutureFrameKind"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    /// The REAL C3 scenario, not just a synthetic top-level tag: the OUTER frame type
    /// (`"Chunk"`) is perfectly recognized, but its nested `ResponseChunk` payload carries an
    /// inner variant this crate's version has never seen. The whole outer frame must degrade
    /// to `Unknown`, tagged with the OUTER type — never accept a partially-decoded chunk.
    #[test]
    fn future_inner_variant_inside_known_outer_frame_degrades_to_unknown_tagged_with_outer_type() {
        let json = r#"{"type":"Chunk","data":{"session_id":"00000000-0000-0000-0000-000000000000","chunk":{"type":"FutureKind","data":{}}}}"#;
        let body: ServerBody = serde_json::from_str(json).expect("must degrade, not hard-fail");
        match body {
            ServerBody::Unknown { type_tag } => assert_eq!(type_tag, "Chunk"),
            other => panic!("expected Unknown{{type_tag:\"Chunk\"}}, got {other:?}"),
        }
    }

    #[test]
    fn missing_type_tag_is_an_error_not_a_panic() {
        let json = r#"{"data":{"x":1}}"#;
        let result: Result<ServerBody, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "missing type tag must error, not decode to a variant"
        );
    }

    #[test]
    fn null_data_on_a_known_tag_degrades_to_unknown() {
        let json = r#"{"type":"Chunk","data":null}"#;
        let body: ServerBody = serde_json::from_str(json).expect("must degrade, not hard-fail");
        match body {
            ServerBody::Unknown { type_tag } => assert_eq!(type_tag, "Chunk"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn malformed_data_shape_on_a_known_tag_degrades_to_unknown() {
        let json = r#"{"type":"Chunk","data":[1,2,3]}"#;
        let body: ServerBody = serde_json::from_str(json).expect("must degrade, not hard-fail");
        match body {
            ServerBody::Unknown { type_tag } => assert_eq!(type_tag, "Chunk"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn non_string_type_tag_is_an_error_not_a_panic() {
        let json = r#"{"type":123,"data":{}}"#;
        let result: Result<ServerBody, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "a non-string type tag must error, not panic or silently decode"
        );
    }

    #[test]
    fn known_variant_still_roundtrips_through_the_shadow_decode() {
        let body = ServerBody::HelloAck {
            epoch: 1,
            protocol_version: crate::PROTOCOL_VERSION,
            kill_on: false,
            mobile_approval_policy: MobileApprovalPolicy::BiometricRequired,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        let round: ServerBody = serde_json::from_str(&json).expect("deserialize");
        match round {
            ServerBody::HelloAck {
                epoch,
                protocol_version,
                kill_on,
                mobile_approval_policy,
            } => {
                assert_eq!(epoch, 1);
                assert_eq!(protocol_version, crate::PROTOCOL_VERSION);
                assert!(!kill_on);
                assert_eq!(
                    mobile_approval_policy,
                    MobileApprovalPolicy::BiometricRequired
                );
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }

    /// Proves the envelope reuses the GUI's exact `ResponseChunk`/`Notification` wire shapes
    /// rather than re-deriving parallel mobile-only versions.
    #[test]
    fn wraps_existing_response_chunk_and_notification_shapes_unchanged() {
        let session_id = Uuid::new_v4();
        let body = ServerBody::Chunk {
            session_id,
            chunk: ResponseChunk::Text("hi".to_string()),
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(json.contains("\"type\":\"Text\""));
        let round: ServerBody = serde_json::from_str(&json).expect("deserialize");
        match round {
            ServerBody::Chunk {
                session_id: sid,
                chunk: ResponseChunk::Text(t),
            } => {
                assert_eq!(sid, session_id);
                assert_eq!(t, "hi");
            }
            other => panic!("expected Chunk/Text, got {other:?}"),
        }

        let notify_body = ServerBody::Notify(Notification::Alert {
            title: "t".into(),
            body: "b".into(),
            urgent: true,
        });
        let json = serde_json::to_string(&notify_body).expect("serialize");
        let round: ServerBody = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            round,
            ServerBody::Notify(Notification::Alert { urgent: true, .. })
        ));
    }

    #[test]
    fn proactive_list_roundtrips_existing_card_shape() {
        let card = ProactiveCard {
            id: Uuid::nil(),
            created_at: "2026-07-12T00:00:00Z".into(),
            kind: ProactiveCardKind::Alert {
                title: "t".into(),
                body: "b".into(),
                urgent: false,
            },
        };
        let body = ServerBody::ProactiveList(vec![card]);
        let json = serde_json::to_string(&body).expect("serialize");
        let round: ServerBody = serde_json::from_str(&json).expect("deserialize");
        match round {
            ServerBody::ProactiveList(cards) => assert_eq!(cards.len(), 1),
            other => panic!("expected ProactiveList, got {other:?}"),
        }
    }
}
