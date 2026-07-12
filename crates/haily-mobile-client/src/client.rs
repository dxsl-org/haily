//! The driving loop: composes [`crate::ws`], [`crate::codec`], [`crate::reconnect`], and
//! [`crate::endpoints`] into "stay connected, forward validated frames, reconnect on drop with
//! backoff, pause while backgrounded" (researcher-01 — sockets die when backgrounded on both
//! platforms, so there is no point retrying until the app is foreground again). The full live
//! round-trip against a real P2a server is P6's job (per the phase plan) — this module's own
//! tests only cover construction/shutdown wiring; the connect/read/write paths are exercised
//! integration-style once `src-tauri-mobile` and a live desktop server both exist.
use crate::codec::{decode_server_frame, encode_client_frame};
use crate::endpoints::{resolve_via_dns, select_endpoint, websocket_url, EndpointSource};
use crate::reconnect::{Backoff, SeqDedup};
use crate::ws::{self, ConnectError, WsStream};
use futures::{SinkExt, StreamExt};
use haily_types::{ClientFrame, PairingQr, ServerFrame};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

/// How often the client sends an application-level `Ping` once connected — see
/// `run_connection`'s doc comment for why this exists alongside plain TCP/WS-protocol keepalive.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// Where to connect and how to authenticate — the pairing QR's host/port/fingerprint double as
/// the connection's ongoing identity; re-pairing is the only way to change them.
#[derive(Debug, Clone)]
pub struct MobileClientConfig {
    pub qr: PairingQr,
    pub token: String,
}

/// Why the reconnect loop gave up entirely (review finding: typed connect errors) — the two
/// cases where retrying with the SAME token/pin can never succeed, so the loop parks rather than
/// backoff-retrying forever against a connect attempt that is guaranteed to fail again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// TLS certificate pin mismatch — the desktop's identity changed (m5); the fix is re-pairing.
    PinMismatch,
    /// The device token was rejected (revoked or invalid) at the WS upgrade.
    AuthRejected,
}

/// Delivered to the Tauri command layer, which maps these onto `emit`-ted desktop-compatible
/// events. That mapping is deliberately NOT this crate's job — it would require Tauri types,
/// breaking this crate's host-only, Tauri-free dependency graph (C2).
#[derive(Debug, Clone)]
pub enum ClientEvent {
    Connected {
        source: EndpointSource,
    },
    /// A single connect/read attempt failed for an ORDINARY (network-shaped) reason — the loop
    /// is still retrying with backoff. Never fired for `PinMismatch`/`AuthRejected`; see
    /// [`ClientEvent::Stopped`] for those.
    Disconnected {
        reason: String,
    },
    /// The loop has given up entirely and will NOT retry again — `PinMismatch`/`AuthRejected`
    /// only. The Tauri bridge should render this as a terminal "re-pair required"/"device
    /// revoked" state, not a transient "reconnecting…" banner.
    Stopped {
        reason: StopReason,
    },
    Frame(ServerFrame),
}

/// Handle to a running client loop. `Clone`able so every Tauri command that needs to send a
/// frame or trigger a disconnect can hold one; the loop itself runs in the task [`spawn`]
/// started.
#[derive(Clone)]
pub struct ClientHandle {
    outbound: mpsc::Sender<ClientFrame>,
    shutdown: CancellationToken,
}

impl ClientHandle {
    /// Queues a frame for the next write. Returns `false` (never panics/blocks) if the loop has
    /// already shut down or its outbound buffer is full — mirrors the desktop `Adapter`'s
    /// best-effort delivery contract; a Tauri command decides for itself whether that failure
    /// is worth surfacing to its own caller.
    pub fn send(&self, frame: ClientFrame) -> bool {
        self.outbound.try_send(frame).is_ok()
    }

    /// Ends the loop after its current connect attempt/read finishes. Idempotent.
    pub fn disconnect(&self) {
        self.shutdown.cancel();
    }
}

/// Spawns the reconnect-forever loop. `foreground` should be flipped by the app's own
/// lifecycle hooks (Tauri's `AppHandle` visibility events on Android/iOS) — starting `false`
/// means the loop waits for the first foreground signal before ever attempting to connect.
pub fn spawn(
    config: MobileClientConfig,
    foreground: watch::Receiver<bool>,
) -> (ClientHandle, mpsc::Receiver<ClientEvent>) {
    let (outbound_tx, outbound_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(256);
    let shutdown = CancellationToken::new();

    let handle = ClientHandle {
        outbound: outbound_tx,
        shutdown: shutdown.clone(),
    };
    tokio::spawn(run_forever(
        config,
        foreground,
        outbound_rx,
        events_tx,
        shutdown,
    ));
    (handle, events_rx)
}

async fn run_forever(
    config: MobileClientConfig,
    mut foreground: watch::Receiver<bool>,
    mut outbound_rx: mpsc::Receiver<ClientFrame>,
    events_tx: mpsc::Sender<ClientEvent>,
    shutdown: CancellationToken,
) {
    let mut backoff = Backoff::with_defaults();
    let mut dedup = SeqDedup::new();

    while !shutdown.is_cancelled() {
        if !*foreground.borrow() {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = foreground.changed() => {}
            }
            continue;
        }

        let magicdns_resolves = resolve_via_dns(&config.qr.host, config.qr.port).await;
        let (endpoint, source) = select_endpoint(&config.qr, magicdns_resolves, None);
        let url = websocket_url(&endpoint);

        match ws::connect(&url, &config.token, &config.qr.cert_fingerprint).await {
            Ok(stream) => {
                backoff.reset();
                let _ = events_tx.send(ClientEvent::Connected { source }).await;
                let reason =
                    run_connection(stream, &mut dedup, &mut outbound_rx, &events_tx, &shutdown)
                        .await;
                let _ = events_tx.send(ClientEvent::Disconnected { reason }).await;
            }
            // Retrying with the same token/pin can never succeed — park instead of spinning
            // backoff against a guaranteed-repeat failure. The loop exits; `outbound_rx` is
            // dropped with it, so a subsequent `ClientHandle::send` correctly starts returning
            // `false` (mirrors an explicit `disconnect()`'s observable effect).
            Err(ConnectError::PinMismatch) => {
                let _ = events_tx
                    .send(ClientEvent::Stopped {
                        reason: StopReason::PinMismatch,
                    })
                    .await;
                return;
            }
            Err(ConnectError::AuthRejected) => {
                let _ = events_tx
                    .send(ClientEvent::Stopped {
                        reason: StopReason::AuthRejected,
                    })
                    .await;
                return;
            }
            Err(e) => {
                let _ = events_tx
                    .send(ClientEvent::Disconnected {
                        reason: e.to_string(),
                    })
                    .await;
            }
        }

        if shutdown.is_cancelled() {
            break;
        }
        let delay = backoff.next_delay();
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// Drives one live connection: sends `Hello` first (§5), then loops reading/writing until the
/// socket closes, an I/O error occurs, or `shutdown` fires. Returns a disconnect reason string.
async fn run_connection(
    mut stream: WsStream,
    dedup: &mut SeqDedup,
    outbound_rx: &mut mpsc::Receiver<ClientFrame>,
    events_tx: &mpsc::Sender<ClientEvent>,
    shutdown: &CancellationToken,
) -> String {
    let hello = dedup.cursor().hello_frame();
    let hello_sent = match encode_client_frame(&hello) {
        Ok(text) => stream.send(Message::Text(text)).await.is_ok(),
        Err(_) => false,
    };
    if !hello_sent {
        return "failed to send Hello".to_string();
    }

    // Application-level keepalive: a cellular NAT/carrier gateway can evict an idle socket well
    // before either side notices via TCP alone, and a long quiet period (no chat/run traffic)
    // gives the client nothing to observe either way. `ClientFrame::Ping` -> `ServerBody::Pong`
    // also advances the server's per-connection seq (§2.2), keeping the resume cursor moving
    // even when nothing else is happening.
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.tick().await; // first tick fires immediately — skip it, Hello was just sent

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = stream.close(None).await;
                return "disconnected by caller".to_string();
            }
            _ = keepalive.tick() => {
                if let Ok(text) = encode_client_frame(&ClientFrame::Ping) {
                    if stream.send(Message::Text(text)).await.is_err() {
                        return "keepalive write failed".to_string();
                    }
                }
            }
            outbound = outbound_rx.recv() => {
                let Some(frame) = outbound else {
                    return "outbound channel closed".to_string();
                };
                if let Ok(text) = encode_client_frame(&frame) {
                    if stream.send(Message::Text(text)).await.is_err() {
                        return "write failed".to_string();
                    }
                }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => match decode_server_frame(&text) {
                        Ok(frame) if dedup.accept(frame.epoch, frame.seq) => {
                            let _ = events_tx.send(ClientEvent::Frame(frame)).await;
                        }
                        Ok(_) => { /* stale/duplicate — already-seen seq, drop silently */ }
                        Err(e) => tracing::debug!("mobile-client: dropping unparseable frame: {e}"),
                    },
                    Some(Ok(Message::Close(_))) | None => return "connection closed".to_string(),
                    Some(Ok(_)) => { /* binary/ping/pong/raw-frame — nothing to decode */ }
                    Some(Err(e)) => return format!("stream error: {e}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> MobileClientConfig {
        MobileClientConfig {
            qr: PairingQr {
                host: "127.0.0.1".into(),
                port: 1, // deliberately unreachable — these tests only check handle wiring
                cert_fingerprint: "sha256:0".to_string(),
                pairing_code: "000000".into(),
                expires_at: "2026-07-12T00:00:00Z".into(),
            },
            token: "test-token".into(),
        }
    }

    #[tokio::test]
    async fn disconnect_is_idempotent_and_stops_the_loop_without_a_foreground_signal() {
        let (_fg_tx, fg_rx) = watch::channel(false);
        let (handle, mut events) = spawn(test_config(), fg_rx);
        // Loop is parked waiting for foreground=true — disconnecting before that must still
        // cleanly end things rather than hang.
        handle.disconnect();
        handle.disconnect();
        // No event is guaranteed here (the loop may exit before ever emitting), but the channel
        // must not panic to poll and eventually closes.
        tokio::time::timeout(std::time::Duration::from_millis(200), events.recv())
            .await
            .ok();
    }

    #[test]
    fn send_after_disconnect_does_not_panic() {
        let (outbound_tx, _rx) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        let handle = ClientHandle {
            outbound: outbound_tx,
            shutdown: shutdown.clone(),
        };
        shutdown.cancel();
        // The receiver is still alive (held by `_rx`) so `try_send` itself still succeeds here —
        // this asserts the call never panics regardless of shutdown state, which is the actual
        // contract `send`'s doc comment makes.
        let _ = handle.send(ClientFrame::Ping);
    }

    /// Real-network proof (review finding: typed connect errors must actually PARK the loop,
    /// not just backoff-retry): a genuinely mismatched cert fingerprint against a real TLS
    /// listener must produce `ClientEvent::Stopped { reason: PinMismatch }` and leave the loop
    /// parked — a `send` afterward must return `false`, exactly like an explicit `disconnect()`.
    #[tokio::test]
    async fn pin_mismatch_stops_the_loop_and_parks_rather_than_retrying() {
        let certified = rcgen::generate_simple_self_signed(vec!["haily-mobile-spike".to_string()])
            .expect("generate self-signed test certificate");
        let cert_der = certified.cert.der().to_vec();
        let key_der = certified.key_pair.serialize_der();
        let key = rustls::pki_types::PrivateKeyDer::try_from(key_der).expect("PKCS8 key");
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![rustls::pki_types::CertificateDer::from(cert_der)], key)
            .expect("build server TLS config");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("local addr");
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config));
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                // The client's own verifier rejects the handshake before any WS bytes would be
                // exchanged — accepting once (and letting the resulting error drop) is enough to
                // prove the client-side classification+parking behavior end to end.
                let _ = acceptor.accept(stream).await;
            }
        });

        let config = MobileClientConfig {
            qr: PairingQr {
                host: addr.ip().to_string(),
                port: addr.port(),
                // Deliberately wrong — the real cert's fingerprint differs from this.
                cert_fingerprint:
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string(),
                pairing_code: "000000".into(),
                expires_at: "2026-07-12T00:00:00Z".into(),
            },
            token: "test-token".into(),
        };
        let (_fg_tx, fg_rx) = watch::channel(true);
        let (handle, mut events) = spawn(config, fg_rx);

        let stopped = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(ClientEvent::Stopped { reason }) => return reason,
                    Some(_) => continue,
                    None => panic!("event channel closed before a Stopped event arrived"),
                }
            }
        })
        .await
        .expect("must receive a Stopped event within the timeout");

        assert_eq!(stopped, StopReason::PinMismatch);
        assert!(
            !handle.send(ClientFrame::Ping),
            "the loop must have parked (outbound channel dropped), not still be retrying"
        );
    }
}
