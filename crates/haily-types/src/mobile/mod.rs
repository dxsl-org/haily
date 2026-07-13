//! Mobile thin-client wire protocol (Mobile Thin-Client plan phase 1).
//!
//! One serde definition shared by the desktop server (P2a, encodes `ServerFrame`/decodes
//! `ClientFrame`) and the mobile Tauri client (P3, decodes `ServerFrame`/encodes
//! `ClientFrame`) — the drift-prevention seam. Full prose contract, sequence diagrams, and
//! threat model live in `docs/mobile-protocol.md`; this module is the executable half of
//! that same contract.
//!
//! Forward-compat (red team C3): `ResponseChunk`/`RunEvent` are closed enums with no
//! catch-all, so a bare wire format would hard-fail an old client the moment the server
//! gains a variant — inevitable given app-store update lag. [`ServerBody`] and
//! [`ClientFrame`] each carry a hand-written `Deserialize` (see their modules) that degrades
//! an unrecognized `type` tag to an `Unknown` variant instead of erroring; `#[serde(other)]`
//! was tried first and rejected — verified by probe test that serde requires the `other`
//! arm to be a unit variant, so it cannot carry a new variant's arbitrary `data` payload.

mod client_frame;
mod pairing;
mod server_body;

pub use client_frame::ClientFrame;
pub use pairing::{PairRequest, PairResponse, PairingQr};
pub use server_body::ServerBody;

use crate::{DepthMode, TranscriptEntry};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Wire protocol version. Bumped ONLY on an envelope-structure change (adding/removing a
/// field on [`ServerFrame`]/[`ClientFrame`] itself, or changing the tag/content shape) —
/// never for a mere new [`ServerBody`]/[`ClientFrame`] variant, which old clients already
/// tolerate via the `Unknown` decode path. See `docs/mobile-protocol.md` § Version
/// Negotiation for the server's hard-block policy on mismatch.
pub const PROTOCOL_VERSION: u16 = 1;

/// The server→client wire envelope. Every frame the server emits — chunk, run event,
/// notification, control frame — is wrapped in exactly this shape; there is no bare/unwrapped
/// server frame (red team C3/C4/M8).
///
/// `epoch` is the server's per-boot nonce (C4): a restarted server's in-memory seq counter
/// resets to 0, and without a generation token those low seq values would all be `<=` a
/// reconnecting client's stored cursor, so the client's dedup logic would silently discard
/// every live frame while looking connected. On `epoch` mismatch the client MUST reset its
/// cursor and treat the connection as a fresh resync (see `docs/mobile-protocol.md` § Resume
/// Semantics), never attempt seq-based reconciliation across an epoch boundary.
///
/// `seq` is monotonic PER CONNECTION (M8) — one counter shared by every frame type
/// (`Chunk`, `Run`, `Notify`, `ProactiveList`, `KillState`, `Pong`, …), assigned by the
/// single writer task that owns the connection's outbound ring buffer (M9). A control frame
/// with no session-scoped payload (`Pong`, `KillState`, `HelloAck`) still consumes a seq
/// slot — the cursor must advance even during periods with no chat/run traffic, or a client
/// that only tracks `Chunk`/`Run` seqs could time out waiting for a gap that was actually a
/// quiet period, not a drop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerFrame {
    pub epoch: u64,
    pub seq: u64,
    pub body: ServerBody,
}

/// Preference gating remote approval authority (red team M1) — a stolen unlocked phone must
/// not silently inherit full Safe-Operator operator authority. Read by the server before
/// honoring a `ClientFrame::Approve` for a `High`/`IrreversibleWrite` tool; enforcement lives
/// in P2a, this is the wire/preference *shape* only.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MobileApprovalPolicy {
    /// Remote approval works exactly like the desktop GUI — no extra gate.
    Allow,
    /// Default. A `High`/`IrreversibleWrite` approval requires `Approve.biometric_ok == true`
    /// (the phone's own OS-level biometric/passcode prompt, checked BEFORE the frame is sent);
    /// the server rejects such an `Approve` with `biometric_ok == false` as a deny.
    #[default]
    BiometricRequired,
    /// Remote can never approve an `IrreversibleWrite` tool at all, regardless of
    /// `biometric_ok` — only the desktop GUI/CLI can.
    DenyIrreversible,
}

/// Server-side error codes returned either over HTTP (pairing) or as `ServerBody::Error`
/// (post-connect). A closed set is safe here (unlike the frame enums): a client that doesn't
/// recognize a code can still fall back to a generic "something went wrong" render — no
/// forward-compat `Unknown` arm needed because failing to render a NEW error's specific copy
/// is a UX nit, not the silent-data-loss failure mode C3 exists to prevent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MobileError {
    PairingCodeInvalid,
    PairingCodeExpired,
    PairingRateLimited,
    PairingNotConfirmed,
    AuthRejected,
    SessionUnknown,
    ProtocolVersion,
    ResumeWindowExceeded,
    Internal,
}

/// The defined response to `ClientFrame::FetchSession` (red team M7) — recovers chat/turn
/// state after a `resume-window-exceeded` reconnect, where seq-based replay can no longer
/// cover the gap. Bounded rather than the full history: built from the same session-transcript
/// seam `haily-app::session_transcript::DbSessionTranscript` already exposes for the ACP
/// channel (phase 12), so this is a second consumer of an existing read path, not a new one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: Uuid,
    /// Chronological (oldest first), bounded by the transcript seam's own replay limit.
    pub transcript: Vec<TranscriptEntry>,
    /// The most recent pipeline run's terminal state for this session, if any is known
    /// (`RunEvent::RunComplete`'s `outcome` string) — lets the client show "your last request
    /// finished" instead of silently dropping a run that completed while disconnected.
    pub latest_run_status: Option<String>,
    /// Judgment depth in effect for this session at snapshot time — reused so a client that
    /// reconnects mid-turn can render the same depth badge it would have shown live.
    #[serde(default)]
    pub depth: DepthMode,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Request, RequestOrigin};

    #[test]
    fn server_frame_envelope_roundtrips_epoch_and_seq() {
        let frame = ServerFrame {
            epoch: 42,
            seq: 7,
            body: ServerBody::Pong,
        };
        let json = serde_json::to_string(&frame).expect("serialize");
        let round: ServerFrame = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round.epoch, 42);
        assert_eq!(round.seq, 7);
        assert!(matches!(round.body, ServerBody::Pong));
    }

    #[test]
    fn mobile_error_uses_snake_case_wire_codes() {
        let json = serde_json::to_string(&MobileError::ResumeWindowExceeded).expect("serialize");
        assert_eq!(json, "\"resume_window_exceeded\"");
        let round: MobileError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round, MobileError::ResumeWindowExceeded);
    }

    #[test]
    fn mobile_approval_policy_defaults_to_biometric_required() {
        assert_eq!(
            MobileApprovalPolicy::default(),
            MobileApprovalPolicy::BiometricRequired
        );
        let json =
            serde_json::to_string(&MobileApprovalPolicy::DenyIrreversible).expect("serialize");
        assert_eq!(json, "\"deny-irreversible\"");
    }

    #[test]
    fn session_snapshot_roundtrips_with_bounded_transcript() {
        let sid = Uuid::new_v4();
        let snapshot = SessionSnapshot {
            session_id: sid,
            transcript: vec![TranscriptEntry {
                role: "user".into(),
                content: "hi".into(),
            }],
            latest_run_status: Some("done".into()),
            depth: DepthMode::Deep,
        };
        let body = ServerBody::SessionSnapshot(snapshot);
        let json = serde_json::to_string(&body).expect("serialize");
        let round: ServerBody = serde_json::from_str(&json).expect("deserialize");
        match round {
            ServerBody::SessionSnapshot(s) => {
                assert_eq!(s.session_id, sid);
                assert_eq!(s.transcript.len(), 1);
                assert_eq!(s.latest_run_status.as_deref(), Some("done"));
                assert_eq!(s.depth, DepthMode::Deep);
            }
            other => panic!("expected SessionSnapshot, got {other:?}"),
        }
    }

    /// Risk Assessment row 4: a remote/wire payload can never set `Request::origin` to `Cli` —
    /// `#[serde(skip)]` means the field isn't even read from an incoming payload, so injecting
    /// the key is a no-op, not an accepted override. Exercised here (rather than only in
    /// `lib.rs`) because this is exactly the mobile threat scenario the phase's risk register
    /// names.
    #[test]
    fn wire_payload_cannot_inject_cli_origin_into_request() {
        let malicious = r#"{"session_id":"00000000-0000-0000-0000-000000000000","adapter_id":"mobile","message":"hi","user_ref":null,"origin":"Cli"}"#;
        let req: Request = serde_json::from_str(malicious)
            .expect("must still deserialize (skip field is ignored)");
        assert_eq!(
            req.origin,
            RequestOrigin::Chat,
            "a wire payload must never set origin to Cli"
        );
    }
}
