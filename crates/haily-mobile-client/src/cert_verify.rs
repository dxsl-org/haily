//! Pinned-fingerprint TLS verifier (Mobile Thin-Client plan phase 3, GATING spike — M13).
//!
//! The desktop mobile server (P2a) is self-signed (`haily-io::mobile::tls`); the WebView
//! cannot open its own sockets at all (M14) and the OS trust store has no reason to know
//! about a home server's certificate, so the client cannot use the platform's default
//! `ServerCertVerifier` (which chains to a root store) — it must instead accept EXACTLY the
//! one certificate whose SHA-256 fingerprint the pairing QR carried (`PairingQr.cert_fingerprint`,
//! same `"sha256:<hex>"` form `haily-io::mobile::tls::fingerprint_of` produces) and hard-fail on
//! anything else, with NO fallback to a CA chain (researcher-03 §6.5 — no bypass button).
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Mirrors `haily-io::mobile::tls::fingerprint_of` exactly — both sides must hash the same
/// bytes (the leaf cert's DER encoding) into the same `"sha256:<hex>"` string, or a
/// legitimately-paired device would spuriously fail this check.
fn fingerprint_of(cert_der: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    format!("sha256:{:x}", hasher.finalize())
}

/// A [`ServerCertVerifier`] that accepts exactly one pinned certificate (by SHA-256
/// fingerprint of its DER encoding) and rejects every other — including a certificate signed
/// by a real, publicly-trusted CA. There is deliberately no fallback path: a fingerprint
/// mismatch always means "re-pair", never "trust anyway".
#[derive(Debug)]
pub struct PinnedCertVerifier {
    /// The exact `"sha256:<hex>"` string pinned at pairing time (`PairingQr.cert_fingerprint`).
    fingerprint: String,
    /// Real signature-verification algorithms (not bypassed) — a pinned fingerprint alone
    /// would only prove "the presented cert's bytes match"; without also verifying the
    /// handshake signature against that cert's public key, an attacker who merely captured
    /// the public cert bytes (they're not secret) could still forge the handshake without
    /// holding the matching private key.
    provider: Arc<CryptoProvider>,
}

impl PinnedCertVerifier {
    pub fn new(fingerprint: String, provider: Arc<CryptoProvider>) -> Self {
        Self {
            fingerprint,
            provider,
        }
    }
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        if fingerprint_of(end_entity) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            // Deliberately generic on the wire/log side (no chain-of-trust details to leak) —
            // the caller-visible consequence is the "desktop identity changed — re-pair" banner
            // (docs/mobile-protocol.md §4), never a bypass prompt.
            Err(RustlsError::General(
                "mobile: pinned certificate fingerprint mismatch — desktop identity changed"
                    .to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Builds a `rustls::ClientConfig` that trusts EXACTLY `fingerprint` and nothing else — no
/// root store, no client auth. Explicitly threads a `ring` [`CryptoProvider`] instance rather
/// than relying on `ClientConfig::builder()`'s process-wide default (`CryptoProvider::install_default`),
/// since `src-tauri-mobile` links other crates (e.g. `reqwest`) that may install a DIFFERENT
/// default provider (`aws-lc-rs`) first — depending on "whichever installed first" would make
/// this config's crypto backend a race, not a deliberate choice.
pub fn pinned_client_config(fingerprint: String) -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCertVerifier::new(fingerprint, provider.clone()));
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring provider supports rustls's default TLS protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Arc::new(config)
}

#[cfg(test)]
mod tests {
    //! GATING spike (phase 3 §Requirements 0): proves `PinnedCertVerifier` works against a REAL
    //! TLS handshake — not just a direct method call — over a real loopback TCP connection, with
    //! a self-signed certificate generated fresh by `rcgen` (mirrors `haily-io`'s own cert
    //! generation exactly). This is the test that gates the rest of phase 3: it only ever ran
    //! green against rustls 0.23.41 / tokio-tungstenite 0.24.0 (the versions the workspace
    //! resolved, per `Cargo.lock`) — see the phase file's Deviation Log for the recorded shape.
    use super::*;
    use rustls::pki_types::PrivateKeyDer;
    use rustls::ServerConfig;
    use std::io;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    struct GeneratedIdentity {
        fingerprint: String,
        server_config: Arc<ServerConfig>,
    }

    fn generate_identity() -> GeneratedIdentity {
        let certified = rcgen::generate_simple_self_signed(vec!["haily-mobile-spike".to_string()])
            .expect("generate self-signed spike certificate");
        let cert_der = certified.cert.der().to_vec();
        let key_der = certified.key_pair.serialize_der();
        let fingerprint = fingerprint_of(&cert_der);

        let key = PrivateKeyDer::try_from(key_der).expect("PKCS8 key from rcgen");
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(cert_der)], key)
            .expect("build server TLS config from the generated identity");

        GeneratedIdentity {
            fingerprint,
            server_config: Arc::new(server_config),
        }
    }

    /// Runs one TLS accept + a fixed handshake-proof byte, then closes. Spawned once per
    /// client attempt below (a fresh TCP connection each time), so each assertion gets an
    /// independent, deterministic accept loop rather than sharing mutable state across cases.
    async fn serve_one(listener: TcpListener, server_config: Arc<ServerConfig>) {
        let (stream, _) = listener.accept().await.expect("accept spike connection");
        let acceptor = TlsAcceptor::from(server_config);
        match acceptor.accept(stream).await {
            Ok(mut tls) => {
                let _ = tls.write_all(b"ok").await;
            }
            Err(_) => { /* expected for the mismatched-fingerprint case below */ }
        }
    }

    #[tokio::test]
    async fn pinned_verifier_accepts_the_exact_fingerprint_over_a_real_handshake() {
        let identity = generate_identity();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind spike listener");
        let addr = listener.local_addr().expect("listener local addr");
        let server_task = tokio::spawn(serve_one(listener, identity.server_config.clone()));

        let client_config = pinned_client_config(identity.fingerprint.clone());
        let connector = TlsConnector::from(client_config);
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect spike TCP stream");
        let server_name = ServerName::try_from("haily-mobile-spike")
            .expect("static DNS name")
            .to_owned();

        let mut tls = connector
            .connect(server_name, tcp)
            .await
            .expect("handshake must succeed for the correctly-pinned fingerprint");
        let mut buf = [0u8; 2];
        tls.read_exact(&mut buf)
            .await
            .expect("read the handshake-proof byte");
        assert_eq!(&buf, b"ok");

        server_task.await.expect("server task must not panic");
    }

    #[tokio::test]
    async fn pinned_verifier_hard_fails_a_mismatched_fingerprint_no_fallback() {
        let identity = generate_identity();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind spike listener");
        let addr = listener.local_addr().expect("listener local addr");
        let server_task = tokio::spawn(serve_one(listener, identity.server_config.clone()));

        // A syntactically-valid but WRONG fingerprint — simulates an attacker's own
        // self-signed cert, or a stale pin after the desktop regenerated its identity (m5).
        let wrong_fingerprint = fingerprint_of(b"not the real certificate bytes");
        assert_ne!(wrong_fingerprint, identity.fingerprint);

        let client_config = pinned_client_config(wrong_fingerprint);
        let connector = TlsConnector::from(client_config);
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect spike TCP stream");
        let server_name = ServerName::try_from("haily-mobile-spike")
            .expect("static DNS name")
            .to_owned();

        let result = connector.connect(server_name, tcp).await;
        match &result {
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::InvalidData
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::Other
                ) => {}
            other => panic!("a mismatched fingerprint must hard-fail the handshake, got {other:?}"),
        }

        // The server side sees the client abort the handshake and never gets to write its
        // proof byte — awaiting the task just proves it exited cleanly (no panic on our side).
        server_task.await.expect("server task must not panic");
    }

    #[test]
    fn fingerprint_of_matches_haily_io_tls_format() {
        // Locks the wire-compatible shape ("sha256:<64 lowercase hex chars>") independent of
        // the handshake tests above — a format drift here would silently break every pin.
        let fp = fingerprint_of(b"anything");
        assert!(fp.starts_with("sha256:"));
        assert_eq!(fp.len(), "sha256:".len() + 64);
        assert!(fp["sha256:".len()..].chars().all(|c| c.is_ascii_hexdigit()));
    }
}
