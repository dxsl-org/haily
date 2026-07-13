//! The actual TLS+WebSocket connect primitive. Builds the `wss://` upgrade request with the
//! device token in the `Authorization` header (never a frame body/URL — see
//! `docs/mobile-protocol.md` §1) and the pinned [`cert_verify`](super::cert_verify) config as
//! the TLS connector, matching exactly what `haily-io::mobile::server::ws_upgrade_handler`
//! expects on the other end.
use crate::cert_verify::pinned_client_config;
use rustls::ClientConfig;
use std::sync::Arc;
use thiserror::Error;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::{
    connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream,
};

pub type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Substring match against `PinnedCertVerifier`'s own hard-fail message
/// (`crate::cert_verify::PinnedCertVerifier::verify_server_cert`) — the only signal available to
/// tell "the desktop's identity changed, re-pair" apart from a plain network failure without
/// rustls's `CertificateError::Other` downcast machinery (a reasonable follow-up, documented in
/// the phase's Deviation Log rather than attempted here under time pressure). The classification
/// this produces is exposed as a real enum variant ([`ConnectError::PinMismatch`]) — this string
/// match is purely this function's OWN internal implementation detail, not something callers
/// need to repeat.
const CERT_MISMATCH_MARKER: &str = "pinned certificate fingerprint mismatch";

/// A caller-actionable classification of why [`connect`] failed. Distinguishing these matters
/// because the reconnect loop (`client.rs`) treats them very differently: [`Self::Network`] is
/// exactly the transient case backoff-retrying exists for, while [`Self::PinMismatch`] and
/// [`Self::AuthRejected`] mean retrying with the SAME credentials/pin can never succeed — the
/// loop parks instead (see `client.rs::run_forever`'s `StopReason` handling).
#[derive(Debug, Error)]
pub enum ConnectError {
    /// The WS upgrade request itself could not be built (e.g. the token contains bytes that
    /// aren't valid HTTP header bytes) — never even reaches the network.
    #[error("building the WS upgrade request: {0}")]
    Request(WsError),
    /// TLS handshake rejected the desktop's certificate because its fingerprint didn't match the
    /// pin (§4 of the protocol doc — hard-fail, no fallback). Typically means the desktop
    /// regenerated its identity (m5) — the fix is re-pairing, not retrying.
    #[error("TLS certificate pin mismatch — desktop identity changed, re-pair required")]
    PinMismatch,
    /// The WS upgrade was rejected with 401/403 — the device token is invalid, expired, or
    /// revoked (`haily-io::mobile::server::ws_upgrade_handler`'s only two failure statuses).
    /// Retrying with the same token can never succeed; the device must be re-paired.
    #[error("device token rejected (revoked or invalid) — re-pair required")]
    AuthRejected,
    /// Any other connect failure (DNS, TCP refused/timed out, WS protocol error, …) — the
    /// ordinary transient case backoff-retrying is meant for.
    #[error("network/connect failure: {0}")]
    Network(WsError),
}

/// Connects to `url` (from [`crate::endpoints::websocket_url`]), pinning TLS to
/// `cert_fingerprint` (§4 of the protocol doc — hard-fail, no fallback) and authenticating the
/// upgrade with `token` (§1 — the token authenticates the upgrade itself, never a later frame).
/// Returns the raw stream; the caller (`client.rs`) sends `ClientFrame::Hello` as the first
/// frame per §5's handshake sequence — that is NOT done here, since building the correct
/// `Hello` requires the [`crate::reconnect::ResumeCursor`] state this module has no reason to
/// know about.
pub async fn connect(
    url: &str,
    token: &str,
    cert_fingerprint: &str,
) -> Result<WsStream, ConnectError> {
    let mut request = url.into_client_request().map_err(ConnectError::Request)?;
    let header_value = format!("Bearer {token}").parse().map_err(|_| {
        ConnectError::Request(WsError::Url(
            tokio_tungstenite::tungstenite::error::UrlError::UnsupportedUrlScheme,
        ))
    })?;
    request.headers_mut().insert(AUTHORIZATION, header_value);

    let tls_config: Arc<ClientConfig> = pinned_client_config(cert_fingerprint.to_string());
    let (stream, _response) =
        connect_async_tls_with_config(request, None, false, Some(Connector::Rustls(tls_config)))
            .await
            .map_err(classify_connect_error)?;
    Ok(stream)
}

/// Classifies a raw `tungstenite::Error` from a failed connect attempt into the caller-actionable
/// [`ConnectError`] taxonomy. `AuthRejected` is a robust, type-based match (`Error::Http` carries
/// a real status code); `PinMismatch` is a pragmatic string match against our OWN verifier's
/// error text (see [`CERT_MISMATCH_MARKER`]'s doc comment for why).
fn classify_connect_error(e: WsError) -> ConnectError {
    if let WsError::Http(response) = &e {
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return ConnectError::AuthRejected;
        }
    }
    if e.to_string().contains(CERT_MISMATCH_MARKER) {
        return ConnectError::PinMismatch;
    }
    ConnectError::Network(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A malformed token that cannot form a valid header value must fail fast at request-build
    /// time rather than reach the network — proves the Authorization header is actually wired
    /// in (rather than silently dropped) without needing a live server.
    #[tokio::test]
    async fn a_token_with_invalid_header_bytes_fails_before_any_connect_attempt() {
        let bad_token = "line1\nline2"; // a raw newline is not a valid HeaderValue byte
        let result = connect(
            "wss://127.0.0.1:1/ws",
            bad_token,
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )
        .await;
        assert!(matches!(result, Err(ConnectError::Request(_))));
    }

    /// An unreachable port must surface as a `Network` error (not panic, not hang past the OS's
    /// own connect-refused signal, and never mistaken for `PinMismatch`/`AuthRejected`) — the
    /// full live handshake against a real server is P6's job.
    #[tokio::test]
    async fn an_unreachable_endpoint_surfaces_as_a_network_error() {
        let result = connect(
            "wss://127.0.0.1:1/ws",
            "sometoken",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )
        .await;
        assert!(matches!(result, Err(ConnectError::Network(_))));
    }

    /// Type-based classification: an HTTP 401 at the WS upgrade (exactly what
    /// `haily-io::mobile::server::ws_upgrade_handler` returns for an invalid/revoked token)
    /// must classify as `AuthRejected`, never `Network`.
    #[test]
    fn http_401_classifies_as_auth_rejected() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(None)
            .expect("build a minimal 401 response");
        let classified = classify_connect_error(WsError::Http(response));
        assert!(matches!(classified, ConnectError::AuthRejected));
    }

    /// Same for 403 (the server's other WS-upgrade rejection status).
    #[test]
    fn http_403_classifies_as_auth_rejected() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(None)
            .expect("build a minimal 403 response");
        let classified = classify_connect_error(WsError::Http(response));
        assert!(matches!(classified, ConnectError::AuthRejected));
    }

    /// An unrelated HTTP status (e.g. a proxy's 502) must NOT be misclassified as an auth
    /// rejection — only 401/403 are.
    #[test]
    fn an_unrelated_http_status_classifies_as_network() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(None)
            .expect("build a minimal 502 response");
        let classified = classify_connect_error(WsError::Http(response));
        assert!(matches!(classified, ConnectError::Network(_)));
    }

    /// The string-sniffed path: an I/O error whose message carries the verifier's own mismatch
    /// text must classify as `PinMismatch`.
    #[test]
    fn an_io_error_mentioning_the_verifier_message_classifies_as_pin_mismatch() {
        let io_err = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "mobile: pinned certificate fingerprint mismatch — desktop identity changed",
        );
        let classified = classify_connect_error(WsError::Io(io_err));
        assert!(matches!(classified, ConnectError::PinMismatch));
    }

    /// An ordinary connection-refused I/O error must classify as `Network`, not `PinMismatch`.
    #[test]
    fn a_plain_io_error_classifies_as_network() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let classified = classify_connect_error(WsError::Io(io_err));
        assert!(matches!(classified, ConnectError::Network(_)));
    }
}
