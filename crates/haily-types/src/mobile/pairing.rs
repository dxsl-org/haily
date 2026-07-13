//! Pairing DTOs. Pairing is a plain HTTP request/response — NOT a WS frame — so the desktop
//! server can gate token issuance on an out-of-band confirm (red team M4) before any
//! WebSocket connection exists at all. See `docs/mobile-protocol.md` § Pairing Sequence.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The payload encoded into the desktop's pairing QR code. `cert_fingerprint` lets the phone
/// pin the desktop's TLS certificate on first connect (anti-MITM, red team researcher-03 §4)
/// — the client must refuse to connect if the presented certificate's fingerprint differs.
/// `pairing_code` is single-use and short-TTL (2 minutes, red team M4); `expires_at` is an
/// RFC3339 timestamp so the client can show "expired, rescan" without a round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingQr {
    /// Tailnet MagicDNS name or loopback host the phone should connect to (see M2's bind
    /// policy — never a bare LAN IP by default).
    pub host: String,
    pub port: u16,
    pub cert_fingerprint: String,
    pub pairing_code: String,
    pub expires_at: String,
}

/// `POST /pair` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub pairing_code: String,
    /// User-supplied or device-reported label shown on the desktop's OOB confirm prompt
    /// (red team M4) — display-only, never trusted for auth.
    pub device_name: String,
}

/// `POST /pair` success response. Returned ONLY after the desktop's out-of-band confirm
/// prompt (M4) has been accepted — never on `PairRequest` receipt alone. A rejected/expired/
/// rate-limited/unconfirmed request returns [`super::MobileError`] instead (over the same
/// HTTP endpoint, not this type).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResponse {
    /// Long-lived bearer token placed in the WS upgrade `Authorization` header — never in a
    /// frame body or URL (see `docs/mobile-protocol.md` § Security).
    pub device_token: String,
    pub device_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_qr_roundtrips() {
        let qr = PairingQr {
            host: "myhost.tailnet.ts.net".into(),
            port: 8443,
            cert_fingerprint: "sha256:deadbeef".into(),
            pairing_code: "123456".into(),
            expires_at: "2026-07-12T18:33:00Z".into(),
        };
        let json = serde_json::to_string(&qr).expect("serialize");
        let round: PairingQr = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round.pairing_code, "123456");
        assert_eq!(round.host, "myhost.tailnet.ts.net");
    }

    #[test]
    fn pair_request_roundtrips() {
        let req = PairRequest {
            pairing_code: "123456".into(),
            device_name: "My Phone".into(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let round: PairRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round.device_name, "My Phone");
    }

    #[test]
    fn pair_response_roundtrips() {
        let device_id = Uuid::new_v4();
        let resp = PairResponse {
            device_token: "tok".into(),
            device_id,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let round: PairResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round.device_id, device_id);
    }
}
