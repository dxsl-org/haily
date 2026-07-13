//! C3 forward-compat CI guard — enforces `docs/mobile-protocol.md` §9's version-negotiation
//! policy at compile/test time, not just by convention.
//!
//! Two trip-wires:
//! 1. An EXHAUSTIVE match (no wildcard arm) over every `ServerBody`/`ClientFrame` variant. Rust
//!    refuses to compile a non-exhaustive match, so the moment either enum gains a variant
//!    upstream (`crates/haily-types/src/mobile/{server_body,client_frame}.rs`) without a
//!    matching edit HERE, `cargo build --tests` / `cargo test` fails — forcing the author to
//!    consciously visit this file and decide: a mere new variant needs no `PROTOCOL_VERSION`
//!    bump (old clients already degrade it via `Unknown`, per §9) and a client render arm
//!    (P3/mobile concern); an envelope-STRUCTURE change (new/removed/retyped `ServerFrame`/
//!    `ClientFrame` field) DOES need a bump, which trip-wire 2 below also catches.
//! 2. A pinned `assert_eq!` against a fixed `PROTOCOL_VERSION` constant, so a bump is never
//!    silent even if it happens to land without touching the match above.
use haily_types::{ClientFrame, MobileApprovalPolicy, MobileError, ServerBody, PROTOCOL_VERSION};
use uuid::Uuid;

/// If this fails, `PROTOCOL_VERSION` was bumped — confirm it was for an envelope-STRUCTURE
/// change (§9), then update this constant deliberately (not as a reflex fix).
const EXPECTED_PROTOCOL_VERSION: u16 = 1;

#[test]
fn protocol_version_is_pinned_a_bump_here_must_be_conscious() {
    assert_eq!(
        PROTOCOL_VERSION, EXPECTED_PROTOCOL_VERSION,
        "PROTOCOL_VERSION changed — verify this was an envelope-structure change (§9), not a \
         mere new variant, before updating EXPECTED_PROTOCOL_VERSION"
    );
}

/// Trip-wire 1 for `ServerBody`. NO wildcard arm — a new variant fails this build.
#[test]
fn server_body_variant_set_is_exhaustively_enumerated_here_adding_one_must_touch_this_file() {
    let samples = [
        ServerBody::HelloAck {
            epoch: 0,
            protocol_version: PROTOCOL_VERSION,
            kill_on: false,
            mobile_approval_policy: MobileApprovalPolicy::default(),
        },
        ServerBody::Chunk {
            session_id: Uuid::nil(),
            chunk: haily_types::ResponseChunk::Text("x".into()),
        },
        ServerBody::Run {
            session_id: Uuid::nil(),
            event: haily_types::RunEvent::RunStarted {
                run_id: "r".into(),
                work_item_id: "w".into(),
            },
        },
        ServerBody::Notify(haily_types::Notification::MorningBrief("x".into())),
        ServerBody::ProactiveList(vec![]),
        ServerBody::SessionSnapshot(haily_types::SessionSnapshot {
            session_id: Uuid::nil(),
            transcript: vec![],
            latest_run_status: None,
            depth: haily_types::DepthMode::default(),
        }),
        ServerBody::KillState { on: false },
        ServerBody::Error(MobileError::Internal),
        ServerBody::Pong,
        ServerBody::Unknown {
            type_tag: "x".into(),
        },
    ];
    for sample in samples {
        // The exhaustive match itself is the guard; this loop only proves each arm is reachable
        // (dead-code-free) rather than merely compiling.
        match sample {
            ServerBody::HelloAck { .. }
            | ServerBody::Chunk { .. }
            | ServerBody::Run { .. }
            | ServerBody::Notify(_)
            | ServerBody::ProactiveList(_)
            | ServerBody::SessionSnapshot(_)
            | ServerBody::KillState { .. }
            | ServerBody::Error(_)
            | ServerBody::Pong
            | ServerBody::Unknown { .. } => {}
        }
    }
}

/// Trip-wire 1 for `ClientFrame`. NO wildcard arm — a new variant fails this build.
#[test]
fn client_frame_variant_set_is_exhaustively_enumerated_here_adding_one_must_touch_this_file() {
    let samples = [
        ClientFrame::Hello {
            last_seen_seq: None,
            last_epoch: None,
            protocol_version: PROTOCOL_VERSION,
        },
        ClientFrame::UserMessage {
            session_id: Uuid::nil(),
            message: "x".into(),
            depth: haily_types::DepthMode::default(),
        },
        ClientFrame::Approve {
            approval_id: Uuid::nil(),
            session_id: Uuid::nil(),
            approved: true,
            biometric_ok: true,
        },
        ClientFrame::SetKillSwitch {
            session_id: Uuid::nil(),
            on: true,
        },
        ClientFrame::FetchProactive {
            session_id: Uuid::nil(),
        },
        ClientFrame::FetchSession {
            session_id: Uuid::nil(),
        },
        ClientFrame::CancelTurn {
            session_id: Uuid::nil(),
        },
        ClientFrame::Ping,
        ClientFrame::Unknown {
            type_tag: "x".into(),
        },
    ];
    for sample in samples {
        match sample {
            ClientFrame::Hello { .. }
            | ClientFrame::UserMessage { .. }
            | ClientFrame::Approve { .. }
            | ClientFrame::SetKillSwitch { .. }
            | ClientFrame::FetchProactive { .. }
            | ClientFrame::FetchSession { .. }
            | ClientFrame::CancelTurn { .. }
            | ClientFrame::Ping
            | ClientFrame::Unknown { .. } => {}
        }
    }
}

/// Envelope-level forward-compat (beyond `haily-types`'s own unit tests, which decode
/// `ServerBody` in isolation): a full wire `ServerFrame` carrying an unrecognized body `type`
/// must still decode — `epoch`/`seq` intact, `body` degraded to `Unknown` — proving an old
/// mobile client parsing a REAL envelope (not a bare inner enum) survives a brand-new server
/// frame kind exactly as §9 requires.
#[test]
fn server_frame_envelope_degrades_an_unknown_body_variant_to_unknown_at_the_full_envelope_level() {
    let json = r#"{"epoch":7,"seq":42,"body":{"type":"SomeFutureFrameKindTheClientHasNeverSeen","data":{"anything":"goes"}}}"#;
    let frame: haily_types::ServerFrame =
        serde_json::from_str(json).expect("the envelope must decode even with an unknown body");
    assert_eq!(frame.epoch, 7);
    assert_eq!(frame.seq, 42);
    match frame.body {
        ServerBody::Unknown { type_tag } => {
            assert_eq!(type_tag, "SomeFutureFrameKindTheClientHasNeverSeen")
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
