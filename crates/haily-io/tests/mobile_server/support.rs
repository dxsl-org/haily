//! Shared E2E harness (Mobile Thin-Client plan phase 6) — a plain `tokio-tungstenite` client
//! driving the REAL `MobileAdapter`/axum server; only the seams production code itself injects
//! post-construction (device store, approval resolver, session transcript, orchestrator sender)
//! are fakes. No mock of `MobileAdapter` or its axum wiring exists anywhere in this suite.
use async_trait::async_trait;
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use haily_io::mobile::{MobileAdapter, MobileDeviceStore, MobileServerConfig};
use haily_io::{ApprovalResolver, SessionTranscript, TranscriptEntry};
use haily_types::{ClientFrame, ServerBody, ServerFrame};
use sha2::{Digest, Sha256};
use std::net::{Ipv4Addr, TcpListener as StdTcpListener};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type ConnectError = tokio_tungstenite::tungstenite::Error;

/// Same hashing scheme as `haily-io::mobile::server`'s private `hash_token` /
/// `haily-db::queries::devices::hash_token` — duplicated here (a test harness has no access to
/// either) so a token registered directly with [`FakeDeviceStore`] (bypassing the HTTP pairing
/// ceremony, which is exercised separately in `pairing.rs`) authenticates identically.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Reserve an OS-assigned ephemeral loopback port, then release it immediately so the server
/// can bind it moments later. A tiny TOCTOU window exists (another process could grab the same
/// port first) but is standard practice for this class of test on a local/CI loopback interface
/// within a single test process.
pub fn reserve_ephemeral_port() -> u16 {
    let listener =
        StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind an ephemeral loopback port");
    listener.local_addr().expect("read local_addr").port()
}

/// In-memory `MobileDeviceStore` — tokens keyed by hash, with a mutable revoke flag so tests can
/// flip revocation without a real DB (`haily-db`'s own query tests already cover the persisted
/// implementation; this harness only needs the seam's contract).
#[derive(Default)]
pub struct FakeDeviceStore {
    by_hash: DashMap<String, Uuid>,
    revoked: DashMap<Uuid, ()>,
}

impl FakeDeviceStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a token as belonging to a fresh device id, bypassing the HTTP pairing ceremony
    /// (pairing itself is exercised end-to-end in `pairing.rs`). Returns the minted device id.
    pub fn register(&self, token_hash: &str) -> Uuid {
        let device_id = Uuid::new_v4();
        self.by_hash.insert(token_hash.to_string(), device_id);
        device_id
    }

    pub fn revoke(&self, device_id: Uuid) {
        self.revoked.insert(device_id, ());
    }
}

#[async_trait]
impl MobileDeviceStore for FakeDeviceStore {
    async fn find_active_by_token_hash(&self, token_hash: &str) -> Option<Uuid> {
        let device_id = *self.by_hash.get(token_hash)?;
        if self.revoked.contains_key(&device_id) {
            None
        } else {
            Some(device_id)
        }
    }

    async fn is_revoked(&self, device_id: Uuid) -> bool {
        self.revoked.contains_key(&device_id)
    }

    async fn touch_last_seen(&self, _device_id: Uuid) {}

    async fn create_device(&self, _device_name: &str, token_hash: &str) -> Option<Uuid> {
        Some(self.register(token_hash))
    }
}

/// Records every `resolve()` call `(approval_id, session_id, approved, returned)` — the
/// `returned` value simulates `haily-core::ApprovalBroker`'s real contract: `false` for an
/// approval id the broker no longer considers pending (already resolved elsewhere, or
/// deny-on-timeout expired at 120s), regardless of what `approved` the caller passed in. Seeding
/// `pending` with an id is what "this approval is still live" means; an id NEVER seeded (or
/// already removed) models "expired before the replay/Approve ever arrived" (red team M10).
#[derive(Default)]
pub struct FakeApprovalResolver {
    pending: std::sync::Mutex<std::collections::HashSet<Uuid>>,
    pub calls: std::sync::Mutex<Vec<(Uuid, Uuid, bool, bool)>>,
}

impl FakeApprovalResolver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn seed_pending(&self, approval_id: Uuid) {
        self.pending.lock().unwrap().insert(approval_id);
    }
}

impl ApprovalResolver for FakeApprovalResolver {
    fn resolve(&self, approval_id: Uuid, session_id: Uuid, approved: bool) -> bool {
        let resolved = self.pending.lock().unwrap().remove(&approval_id);
        self.calls
            .lock()
            .unwrap()
            .push((approval_id, session_id, approved, resolved));
        resolved
    }
}

/// A trivial `SessionTranscript` fake — session id (as string) to a fixed transcript.
#[derive(Default)]
pub struct FakeSessionTranscript {
    entries: DashMap<String, Vec<TranscriptEntry>>,
}

impl FakeSessionTranscript {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn seed(&self, session_id: Uuid, entries: Vec<TranscriptEntry>) {
        self.entries.insert(session_id.to_string(), entries);
    }
}

#[async_trait]
impl SessionTranscript for FakeSessionTranscript {
    async fn transcript(&self, session_id: &str) -> Vec<TranscriptEntry> {
        self.entries
            .get(session_id)
            .map(|e| e.clone())
            .unwrap_or_default()
    }
}

/// A running test server: the real `MobileAdapter`, bound to loopback, plus the fakes wired in
/// (mirroring `haily-app::bootstrap`'s post-construction injection contract).
pub struct TestServer {
    pub adapter: MobileAdapter,
    pub port: u16,
    pub devices: Arc<FakeDeviceStore>,
    /// Every `haily_types::Request` the adapter forwarded to "the orchestrator", captured (not
    /// silently discarded) so a test can assert on what actually reached the dispatch boundary —
    /// e.g. m2's remote-Deep-downgrade. Stays empty for tests that never inspect it.
    pub forwarded_requests: Arc<tokio::sync::Mutex<Vec<haily_types::Request>>>,
}

/// Start a `MobileAdapter` bound to an ephemeral loopback port with `overrides` applied to the
/// config before bind. Uses [`MobileAdapter::start_and_await_bind`] (not the fire-and-forget
/// `Adapter::start`) so the caller knows the listener is actually up before issuing requests —
/// no sleep-and-hope needed.
pub async fn start_test_server(overrides: impl FnOnce(&mut MobileServerConfig)) -> TestServer {
    let port = reserve_ephemeral_port();
    let mut config = MobileServerConfig {
        enabled: true,
        port,
        lan_opt_in: false,
        ..MobileServerConfig::default()
    };
    overrides(&mut config);
    let devices = FakeDeviceStore::new();
    let adapter = MobileAdapter::new(config, devices.clone(), std::env::temp_dir());
    bind_and_wire(adapter, devices, port).await
}

/// Same as [`start_test_server`], but the pairing service reads `pairing_clock` instead of the
/// real wall clock (test seam for TTL-expiry assertions — see
/// `MobileAdapter::new_with_pairing_clock`'s doc).
pub async fn start_test_server_with_pairing_clock(
    overrides: impl FnOnce(&mut MobileServerConfig),
    pairing_clock: fn() -> std::time::Instant,
) -> TestServer {
    let port = reserve_ephemeral_port();
    let mut config = MobileServerConfig {
        enabled: true,
        port,
        lan_opt_in: false,
        ..MobileServerConfig::default()
    };
    overrides(&mut config);
    let devices = FakeDeviceStore::new();
    let adapter = MobileAdapter::new_with_pairing_clock(
        config,
        devices.clone(),
        std::env::temp_dir(),
        pairing_clock,
    );
    bind_and_wire(adapter, devices, port).await
}

async fn bind_and_wire(
    adapter: MobileAdapter,
    devices: Arc<FakeDeviceStore>,
    port: u16,
) -> TestServer {
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let forwarded_requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let captured = forwarded_requests.clone();
    // Capture (rather than blindly drain) every forwarded `Request` — cheap, and lets a test
    // assert on it (m2) without needing a separate server-construction path.
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            captured.lock().await.push(req);
        }
    });
    let bound = adapter.start_and_await_bind(tx).await;
    assert!(bound, "test server must successfully bind a loopback port");
    TestServer {
        adapter,
        port,
        devices,
        forwarded_requests,
    }
}

/// Connect to `server`'s `/ws` endpoint with `token` in the `Authorization` header — exactly
/// what `haily-mobile-client::ws::connect` does, minus the TLS pinning (the loopback listener is
/// plain `ws://`, per `bind::requires_tls`).
pub async fn connect_ws(port: u16, token: &str) -> Result<WsStream, ConnectError> {
    let url = format!("ws://127.0.0.1:{port}/ws");
    let mut request = url.into_client_request().expect("build WS upgrade request");
    request.headers_mut().insert(
        AUTHORIZATION,
        format!("Bearer {token}")
            .parse()
            .expect("valid header value"),
    );
    let (stream, _response) = connect_async(request).await?;
    Ok(stream)
}

/// Connect with NO `Authorization` header at all — the "missing header" auth-rejection case.
pub async fn connect_ws_no_auth(port: u16) -> Result<WsStream, ConnectError> {
    let url = format!("ws://127.0.0.1:{port}/ws");
    let request = url.into_client_request().expect("build WS upgrade request");
    let (stream, _response) = connect_async(request).await?;
    Ok(stream)
}

pub async fn send_frame(ws: &mut WsStream, frame: ClientFrame) {
    let json = serde_json::to_string(&frame).expect("serialize ClientFrame");
    ws.send(Message::Text(json))
        .await
        .expect("send ClientFrame");
}

pub async fn send_hello(ws: &mut WsStream, last_seen_seq: Option<u64>, last_epoch: Option<u64>) {
    send_frame(
        ws,
        ClientFrame::Hello {
            last_seen_seq,
            last_epoch,
            protocol_version: haily_types::PROTOCOL_VERSION,
        },
    )
    .await;
}

/// Read the next decodable `ServerFrame`, skipping WS-level control frames (ping/pong/binary)
/// that the app-level protocol never uses. `None` on stream end or decode failure.
pub async fn recv_frame(ws: &mut WsStream) -> Option<ServerFrame> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).ok(),
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue,
            Some(Err(_)) => return None,
        }
    }
}

/// [`recv_frame`] bounded by `timeout` — `None` on timeout too, so a test asserting "the
/// connection is closed / nothing more arrives" can use the same call as one asserting "a frame
/// arrives promptly".
pub async fn recv_frame_timeout(ws: &mut WsStream, timeout: Duration) -> Option<ServerFrame> {
    tokio::time::timeout(timeout, recv_frame(ws))
        .await
        .ok()
        .flatten()
}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// Extract `epoch` from a `HelloAck` frame — panics on any other body (a test-only assertion
/// helper, not production decoding logic).
pub fn expect_epoch(frame: &ServerFrame) -> u64 {
    match frame.body {
        ServerBody::HelloAck { epoch, .. } => epoch,
        ref other => panic!("expected HelloAck, got {other:?}"),
    }
}

/// Extract `kill_on` from a `HelloAck` frame — same test-only-assertion contract as
/// [`expect_epoch`].
pub fn expect_kill_on(frame: &ServerFrame) -> bool {
    match frame.body {
        ServerBody::HelloAck { kill_on, .. } => kill_on,
        ref other => panic!("expected HelloAck, got {other:?}"),
    }
}

/// Connect, send `Hello`, and read frames until (and including) a `HelloAck` is seen — the
/// common setup every test needs, folded into one call.
///
/// Returns EVERY frame read, in order, ending in the `HelloAck` — NOT just the `HelloAck` alone.
/// On a fresh (never-connected) device or a cleanly-tracked resume, that vec is exactly
/// `[HelloAck]`; on a reconnect with a stale/absent cursor, the ring buffer can replay older
/// frames BEFORE the reconnect's own fresh `HelloAck` (the wire contract's "every frame type
/// shares one seq space" — a `HelloAck` is not privileged to arrive first). Callers that only
/// need the handshake's own state (`epoch`/`kill_on`) should read `.last()`; callers proving a
/// resume replay should inspect the earlier entries too.
///
/// NOT used for a resume-window-exceeded reconnect — that path never delivers a `HelloAck` at
/// all on the exhausted connection (see `auth_and_resume.rs`'s overflow test, which connects and
/// reads manually instead of via this helper).
///
/// Panics (via `expect`) if no `HelloAck` arrives within [`DEFAULT_TIMEOUT`] — a hung handshake
/// is a genuine test failure, not a case to swallow.
pub async fn connect_and_handshake(
    port: u16,
    token: &str,
    last_seen_seq: Option<u64>,
    last_epoch: Option<u64>,
) -> (WsStream, Vec<ServerFrame>) {
    let mut ws = connect_ws(port, token)
        .await
        .expect("WS upgrade must succeed");
    send_hello(&mut ws, last_seen_seq, last_epoch).await;
    let mut frames = Vec::new();
    loop {
        let frame = recv_frame_timeout(&mut ws, DEFAULT_TIMEOUT)
            .await
            .expect("HelloAck must arrive during handshake");
        let is_hello_ack = matches!(frame.body, ServerBody::HelloAck { .. });
        frames.push(frame);
        if is_hello_ack {
            return (ws, frames);
        }
    }
}

/// Claim `session_id` for this connection by sending an ordinary `UserMessage` (the same path a
/// real chat turn uses) — the harness's only way to populate `session_owner`, which is
/// `pub(crate)` and therefore invisible to this external test crate. Returns the fencing `Pong`'s
/// seq (see [`fence`]) so the caller can track its resume cursor precisely.
pub async fn claim_session(ws: &mut WsStream, session_id: Uuid) -> u64 {
    send_frame(
        ws,
        ClientFrame::UserMessage {
            session_id,
            message: "hello".to_string(),
            depth: haily_types::DepthMode::Normal,
        },
    )
    .await;
    // Fence: a `Ping` is processed strictly after the `UserMessage` on the same connection's
    // single read loop, so receiving `Pong` proves the claim has already been recorded.
    fence(ws).await
}

/// Send `Ping` and await `Pong`, returning the `Pong`'s seq — proves every frame sent BEFORE
/// this one has already been fully processed by the server's connection loop (frames are
/// handled sequentially, in send order, on one task), without any wall-clock sleep, and gives
/// the caller an exact cursor position for a subsequent clean reconnect.
pub async fn fence(ws: &mut WsStream) -> u64 {
    send_frame(ws, ClientFrame::Ping).await;
    loop {
        let frame = recv_frame_timeout(ws, DEFAULT_TIMEOUT)
            .await
            .expect("Pong must arrive to complete the fence");
        if matches!(frame.body, ServerBody::Pong) {
            return frame.seq;
        }
        // A live push can legitimately interleave before the Pong (e.g. a concurrent
        // `deliver()`); keep reading until the fence's own Pong specifically arrives.
    }
}
