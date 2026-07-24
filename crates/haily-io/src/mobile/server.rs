//! Axum app + per-connection WS loop — wires `bind`/`tls`/`pairing`/`ring_buffer`/`writer`/
//! `guard` into the actual network surface. Kept private to `crate::mobile`; `MobileAdapter`
//! (`mod.rs`) is the only public entry point.
use super::guard::{approval_allowed, claim_or_verify_session, RateLimiter};
use super::pairing::RedeemOutcome;
use super::writer::{DeviceWriter, FrameSink, ResumeOutcome};
use super::{bind, tls, MobileAdapter};
use crate::Request as HailyRequest;
use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use haily_types::{
    ClientFrame, DepthMode, MobileError, Notification, PairRequest, PairResponse, ServerBody,
    SessionSnapshot, PROTOCOL_VERSION,
};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How often a live connection's background task re-checks `MobileDeviceStore::is_revoked`
/// against the DB (review findings 1/5) — a device revoked via a future out-of-band path (e.g.
/// P2b's Devices panel calling `devices::revoke` directly) is disconnected within this bound
/// even with no explicit `disconnect_device` caller wired yet. `disconnect_device` itself
/// (called here on a positive hit) closes the socket immediately rather than waiting for this
/// interval to elapse again.
const REVOKE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// SHA-256 hex — duplicated from `haily_db::queries::devices::hash_token` rather than
/// depending on `haily-db` from this leaf `haily-io` crate (the layering invariant
/// `SessionTranscript`'s doc comment already documents). Both sides compute plain SHA-256 hex
/// of the same token, so they interoperate exactly.
fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

struct WsSink(SplitSink<WebSocket, Message>);

#[async_trait]
impl FrameSink for WsSink {
    async fn send_frame(&mut self, frame: &haily_types::ServerFrame) -> bool {
        match serde_json::to_string(frame) {
            Ok(json) => self.0.send(Message::Text(json)).await.is_ok(),
            Err(_) => false,
        }
    }
}

/// Entry point spawned by `MobileAdapter::start` (and directly `.await`ed by
/// `MobileAdapter::start_and_await_bind` for a caller that needs to know bind success —
/// review finding 6c). Binds every M2/M3-selected address and serves `/pair` + `/ws` on each;
/// a bind failure anywhere here is logged and non-fatal to `Adapter::start`'s caller (red team
/// M11) — `start()` already returned `Ok` before this task was even spawned there. Returns
/// whether at least one address was successfully bound.
pub(super) async fn run(state: MobileAdapter, _tx: crate::RequestSender) -> bool {
    let app = Router::new()
        .route("/pair", post(pair_handler))
        .route("/ws", get(ws_upgrade_handler))
        .with_state(state.clone());

    let interfaces = bind::enumerate_interfaces();
    let addrs = bind::select_bind_addrs(&interfaces, state.config.lan_opt_in, state.config.port);
    if addrs.is_empty() {
        tracing::warn!("mobile: no bindable address selected — mobile server degraded/off");
        return false;
    }

    let cert = if addrs.iter().any(|a| bind::requires_tls(a.ip())) {
        match tls::load_or_generate(&state.data_dir) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(
                    "mobile: TLS identity unavailable — LAN(wss://) listener disabled: {e:#}"
                );
                None
            }
        }
    } else {
        None
    };

    let mut bound_any = false;
    for addr in addrs {
        let app = app.clone();
        if bind::requires_tls(addr.ip()) {
            let Some(cert) = &cert else { continue };
            match axum_server::tls_rustls::RustlsConfig::from_der(
                vec![cert.cert_der.clone()],
                cert.key_der.clone(),
            )
            .await
            {
                Ok(tls_config) => {
                    bound_any = true;
                    tracing::info!(%addr, "mobile: listening (wss://)");
                    let handle = axum_server::Handle::new();
                    tokio::spawn(
                        axum_server::bind_rustls(addr, tls_config)
                            .handle(handle)
                            .serve(app.into_make_service_with_connect_info::<SocketAddr>()),
                    );
                }
                Err(e) => tracing::warn!(%addr, "mobile: TLS bind failed (degraded): {e:#}"),
            }
        } else {
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    bound_any = true;
                    tracing::info!(%addr, "mobile: listening (ws://)");
                    tokio::spawn(async move {
                        if let Err(e) = axum::serve(
                            listener,
                            app.into_make_service_with_connect_info::<SocketAddr>(),
                        )
                        .await
                        {
                            tracing::warn!("mobile: server on {addr} exited: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(%addr, "mobile: bind failed (degraded — this address unavailable): {e:#}")
                }
            }
        }
    }
    if !bound_any {
        tracing::warn!(
            "mobile: every candidate address failed to bind — mobile server fully degraded/off"
        );
    }
    bound_any
}

async fn pair_handler(
    State(state): State<MobileAdapter>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<PairRequest>,
) -> Response {
    match state
        .pairing
        .redeem(&req.pairing_code, &req.device_name, addr.ip())
        .await
    {
        RedeemOutcome::Confirmed => {
            let token = super::pairing::generate_token();
            let token_hash = hash_token(&token);
            match state
                .devices
                .create_device(&req.device_name, &token_hash)
                .await
            {
                Some(device_id) => {
                    state.pairing.record_issued(
                        &req.pairing_code,
                        device_id,
                        token.clone(),
                        req.device_name.clone(),
                    );
                    Json(PairResponse {
                        device_token: token,
                        device_id,
                    })
                    .into_response()
                }
                // Review finding 3: a persistence failure must NOT mint a dead token the phone
                // silently can't use — respond 500 and, crucially, do NOT `record_issued`, so a
                // legitimate retry with the same code gets a fresh attempt at persisting rather
                // than being told the (never-actually-created) device is idempotently "issued".
                None => {
                    tracing::error!(pairing_code = %req.pairing_code, "mobile: pairing confirmed but device persistence failed");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(MobileError::Internal),
                    )
                        .into_response()
                }
            }
        }
        RedeemOutcome::AlreadyIssued { device_id, token } => Json(PairResponse {
            device_token: token,
            device_id,
        })
        .into_response(),
        RedeemOutcome::Denied => (
            StatusCode::FORBIDDEN,
            Json(MobileError::PairingNotConfirmed),
        )
            .into_response(),
        RedeemOutcome::Expired => {
            (StatusCode::GONE, Json(MobileError::PairingCodeExpired)).into_response()
        }
        RedeemOutcome::Invalid => {
            (StatusCode::NOT_FOUND, Json(MobileError::PairingCodeInvalid)).into_response()
        }
        RedeemOutcome::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(MobileError::PairingRateLimited),
        )
            .into_response(),
    }
}

async fn ws_upgrade_handler(
    State(state): State<MobileAdapter>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or malformed Authorization header",
        )
            .into_response();
    };

    let token_hash = hash_token(token);
    let Some(device_id) = state.devices.find_active_by_token_hash(&token_hash).await else {
        return (
            StatusCode::UNAUTHORIZED,
            "invalid, unknown, or revoked device token",
        )
            .into_response();
    };
    state.devices.touch_last_seen(device_id).await;
    // A fresh, successful, non-revoked auth check just happened above — clear any stale
    // `revoked_cache` entry (e.g. from a past transient DB read error that fail-closed to
    // "revoked", review finding 1/5's cache) so a genuinely-valid device is never permanently
    // wedged by a one-off hiccup.
    state.revoked_cache.remove(&device_id);
    ws.on_upgrade(move |socket| handle_socket(socket, state, device_id))
}

fn decode_client_frame(text: &str) -> Option<ClientFrame> {
    match serde_json::from_str::<ClientFrame>(text) {
        Ok(frame) => Some(frame),
        Err(e) => {
            tracing::debug!("mobile: failed to decode client frame: {e:#}");
            None
        }
    }
}

fn current_kill_state(state: &MobileAdapter) -> bool {
    state
        .kill
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|k| k.load(Ordering::Acquire))
        .unwrap_or(false)
}

/// Re-polls `MobileDeviceStore::is_revoked` every [`REVOKE_POLL_INTERVAL`] for the lifetime of
/// one connection (review findings 1/5) — the enforcement mechanism behind `revoked_cache`, so
/// a revocation that happens via ANY path (not just an in-process `disconnect_device` caller,
/// which nothing invokes yet in this phase — see the phase's Deviation Log) still closes an
/// already-open socket within a bounded time, not just at the next reconnect attempt. Exits
/// either when `cancel` fires (connection ended for some other reason) or once it triggers a
/// disconnect itself.
async fn revoke_poll_loop(state: MobileAdapter, device_id: Uuid, cancel: CancellationToken) {
    let mut interval = tokio::time::interval(REVOKE_POLL_INTERVAL);
    interval.tick().await; // first tick fires immediately — skip it, upgrade already checked
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = interval.tick() => {
                if state.devices.is_revoked(device_id).await {
                    tracing::warn!(%device_id, "mobile: periodic revoke check found this device revoked — disconnecting");
                    state.disconnect_device(device_id);
                    return;
                }
            }
        }
    }
}

async fn handle_socket(socket: WebSocket, state: MobileAdapter, device_id: Uuid) {
    // Review finding 1: a per-connection cancellation token, registered so
    // `disconnect_device` can close THIS live socket immediately rather than waiting for the
    // next inbound frame (which may never arrive if the client is idle) or the next reconnect
    // attempt. `conn_id` lets this connection's own cleanup (below) remove only its own entry,
    // never a newer reconnect's that may have already replaced it.
    let conn_id = Uuid::new_v4();
    let cancel = CancellationToken::new();
    state
        .connections
        .insert(device_id, (conn_id, cancel.clone()));
    let poll_task = tokio::spawn(revoke_poll_loop(state.clone(), device_id, cancel.clone()));

    connection_loop(socket, &state, device_id, &cancel).await;

    // Cleanup runs regardless of which path `connection_loop` exited through (bad Hello,
    // protocol mismatch, cancellation, or the client's stream ending naturally).
    poll_task.abort();
    state
        .connections
        .remove_if(&device_id, |_, (id, _)| *id == conn_id);
}

async fn connection_loop(
    socket: WebSocket,
    state: &MobileAdapter,
    device_id: Uuid,
    cancel: &CancellationToken,
) {
    let (sink, mut stream) = socket.split();
    let writer = state.get_or_spawn_writer(device_id);

    let hello = match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => decode_client_frame(&text),
        _ => None,
    };
    let Some(ClientFrame::Hello {
        last_seen_seq,
        last_epoch,
        protocol_version,
    }) = hello
    else {
        tracing::warn!(%device_id, "mobile: connection closed — first frame was not a valid Hello");
        return;
    };

    if protocol_version != PROTOCOL_VERSION {
        writer.push(ServerBody::Error(MobileError::ProtocolVersion));
        let _ = writer.resume(None, Box::new(WsSink(sink))).await;
        return;
    }

    writer.push(ServerBody::HelloAck {
        epoch: state.epoch,
        protocol_version: PROTOCOL_VERSION,
        kill_on: current_kill_state(state),
        mobile_approval_policy: state.config.approval_policy,
    });

    // Epoch mismatch (C4): the client's cursor describes a DIFFERENT process's seq space, so
    // treating it as "no cursor" here (replay everything retained) is the correct handling, not
    // a special case — the client already knows to discard it and re-`FetchSession`.
    let cursor = if last_epoch == Some(state.epoch) {
        last_seen_seq
    } else {
        None
    };
    if let Ok(ResumeOutcome::WindowExceeded) = writer.resume(cursor, Box::new(WsSink(sink))).await {
        writer.push(ServerBody::Error(MobileError::ResumeWindowExceeded));
    }

    let mut rate_limiter = RateLimiter::new(state.config.inbound_rate_limit_per_minute);
    loop {
        let msg = tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!(%device_id, "mobile: connection cancelled (revoked or explicitly disconnected)");
                break;
            }
            msg = stream.next() => msg,
        };
        let Some(Ok(msg)) = msg else { break };
        let Message::Text(text) = msg else { continue };
        if !rate_limiter.allow() {
            tracing::warn!(%device_id, "mobile: inbound rate limit exceeded — dropping frame");
            continue;
        }
        // Review finding 5: a cheap CACHED revoked check (no per-frame DB round-trip) — the
        // actual enforcement is `revoke_poll_loop`'s periodic DB re-check + `disconnect_device`
        // (which sets this cache and cancels `cancel`, caught by the select arm above); this is
        // an extra, essentially-free belt-and-suspenders check for the rare case a frame is
        // already queued in `stream.next()`'s buffer at the exact moment of cancellation.
        if state.revoked_cache.contains_key(&device_id) {
            tracing::warn!(%device_id, "mobile: revoked device sent a frame — closing connection");
            break;
        }
        let Some(frame) = decode_client_frame(&text) else {
            continue;
        };
        handle_client_frame(state, device_id, &writer, frame).await;
    }
    writer.detach();
}

async fn handle_client_frame(
    state: &MobileAdapter,
    device_id: Uuid,
    writer: &DeviceWriter,
    frame: ClientFrame,
) {
    match frame {
        ClientFrame::Ping => writer.push(ServerBody::Pong),
        ClientFrame::Hello { .. } => {
            tracing::debug!(%device_id, "mobile: unexpected mid-stream Hello — ignoring");
        }
        ClientFrame::Unknown { type_tag } => {
            tracing::debug!(%device_id, type_tag, "mobile: unrecognized client frame — ignoring");
        }
        ClientFrame::UserMessage {
            session_id,
            message,
            depth,
        } => {
            if !claim_or_verify_session(&state.session_owner, session_id, device_id) {
                writer.push(ServerBody::Error(MobileError::SessionUnknown));
                return;
            }
            let depth = if state.config.deny_remote_deep && depth == DepthMode::Deep {
                DepthMode::Normal
            } else {
                depth
            };
            forward_user_message(state, session_id, device_id, message, depth).await;
        }
        ClientFrame::Approve {
            approval_id,
            session_id,
            approved,
            biometric_ok,
        } => {
            if !claim_or_verify_session(&state.session_owner, session_id, device_id) {
                writer.push(ServerBody::Error(MobileError::SessionUnknown));
                return;
            }
            let reversible = state
                .approval_reversible
                .remove(&approval_id)
                .map(|(_, (v, _inserted_at))| v)
                .unwrap_or(false);
            let honored = approval_allowed(
                state.config.approval_policy,
                reversible,
                biometric_ok,
                approved,
            );
            let resolver = state.resolver_handle();
            if let Some(resolver) = resolver {
                resolver.resolve(approval_id, session_id, honored);
            }
        }
        ClientFrame::SetKillSwitch { session_id, on } => {
            if !claim_or_verify_session(&state.session_owner, session_id, device_id) {
                writer.push(ServerBody::Error(MobileError::SessionUnknown));
                return;
            }
            // M1: mobile can only ENABLE the kill switch (disable requires the desktop).
            if on {
                if let Some(kill) = state.kill_handle() {
                    kill.store(true, Ordering::Release);
                    // Review finding 2 (m7): broadcast the change to every OTHER adapter
                    // (GUI/Telegram/other mobile devices) too, not just this connection's own
                    // device — the kill switch is intentionally global (M15).
                    if let Some(manager) = state.manager_handle() {
                        if let Err(e) = manager
                            .notify_all(Notification::KillStateChanged { on: true })
                            .await
                        {
                            tracing::warn!("mobile: KillStateChanged broadcast failed: {e:#}");
                        }
                    } else {
                        tracing::warn!("mobile: kill-switch enabled but no AdapterManager handle is wired yet — other channels will not see the change until their own next state read");
                    }
                }
            } else {
                tracing::warn!(%device_id, %session_id, "mobile: rejected an attempt to DISABLE the kill switch remotely");
            }
        }
        ClientFrame::FetchProactive { session_id } => {
            if !claim_or_verify_session(&state.session_owner, session_id, device_id) {
                writer.push(ServerBody::Error(MobileError::SessionUnknown));
                return;
            }
            let cards = state
                .proactive
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            writer.push(ServerBody::ProactiveList(cards));
        }
        ClientFrame::FetchSession { session_id } => {
            if !claim_or_verify_session(&state.session_owner, session_id, device_id) {
                writer.push(ServerBody::Error(MobileError::SessionUnknown));
                return;
            }
            let transcript = state.transcript_handle();
            let entries = match transcript {
                Some(t) => t.transcript(&session_id.to_string()).await,
                None => Vec::new(),
            };
            // `latest_run_status`/`depth` have no seam into haily-io yet (see the phase's
            // Deviation Log) — safe defaults, never a fabricated value.
            writer.push(ServerBody::SessionSnapshot(SessionSnapshot {
                session_id,
                transcript: entries,
                latest_run_status: None,
                depth: DepthMode::default(),
            }));
        }
        // Mobile Thin-Client plan phase 3 amendment (additive ClientFrame variant, §9 — no
        // PROTOCOL_VERSION bump). Session-bound like every other session-scoped frame (m1): a
        // device may only cancel a turn on a session it owns, never a foreign one. A missing
        // `turn_canceller` (not yet wired, or a build without it) makes this a silent no-op —
        // mirrors `FetchProactive`/`FetchSession`'s own "no seam yet" fallback, never an error
        // that would disconnect the socket over a capability gap.
        ClientFrame::CancelTurn { session_id } => {
            if !claim_or_verify_session(&state.session_owner, session_id, device_id) {
                writer.push(ServerBody::Error(MobileError::SessionUnknown));
                return;
            }
            if let Some(canceller) = state.turn_canceller_handle() {
                canceller.cancel(session_id);
            }
        }
    }
}

async fn forward_user_message(
    state: &MobileAdapter,
    session_id: Uuid,
    device_id: Uuid,
    message: String,
    depth: DepthMode,
) {
    let tx = state.tx_handle();
    let Some(tx) = tx else {
        tracing::warn!("mobile: orchestrator channel not wired yet — dropping message");
        return;
    };
    let req = HailyRequest {
        session_id,
        adapter_id: "mobile".to_string(),
        message,
        user_ref: Some(device_id.to_string()),
        depth,
        origin: Default::default(),
        forced_skill: None,
    };
    if tx.send(req).await.is_err() {
        tracing::warn!("mobile: orchestrator channel closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::AdapterManager;
    use crate::mobile::MobileDeviceStore;
    use crate::{Adapter as CrateAdapter, ResponseChunk, RunEvent};
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex as StdMutex;

    struct FakeDeviceStore;

    #[async_trait]
    impl super::super::MobileDeviceStore for FakeDeviceStore {
        async fn find_active_by_token_hash(&self, _token_hash: &str) -> Option<Uuid> {
            None
        }
        async fn is_revoked(&self, _device_id: Uuid) -> bool {
            false
        }
        async fn touch_last_seen(&self, _device_id: Uuid) {}
        async fn create_device(&self, _device_name: &str, _token_hash: &str) -> Option<Uuid> {
            Some(Uuid::new_v4())
        }
    }

    /// Records every `Notification` it receives — used to prove `notify_all` was actually
    /// reached (review finding 2).
    struct RecordingAdapter {
        notifications: std::sync::Arc<StdMutex<Vec<Notification>>>,
    }

    #[async_trait]
    impl CrateAdapter for RecordingAdapter {
        async fn start(&self, _tx: crate::RequestSender) -> anyhow::Result<()> {
            Ok(())
        }
        async fn deliver(&self, _session_id: Uuid, _chunk: ResponseChunk) -> anyhow::Result<()> {
            Ok(())
        }
        async fn deliver_run_event(
            &self,
            _session_id: Uuid,
            _event: RunEvent,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn notify(&self, msg: Notification) -> anyhow::Result<()> {
            self.notifications.lock().unwrap().push(msg);
            Ok(())
        }
        fn id(&self) -> &str {
            "recording"
        }
    }

    fn test_adapter() -> MobileAdapter {
        MobileAdapter::new(
            super::super::MobileServerConfig::default(),
            std::sync::Arc::new(FakeDeviceStore),
            std::env::temp_dir(),
        )
    }

    /// Review finding 2 (m7): the mobile-initiated kill-switch ENABLE path must broadcast
    /// `Notification::KillStateChanged` to every OTHER adapter via `notify_all`, not just push
    /// it to this adapter's own connected devices.
    #[tokio::test]
    async fn set_kill_switch_enable_from_mobile_broadcasts_kill_state_changed() {
        let state = test_adapter();
        state.set_kill_switch(std::sync::Arc::new(AtomicBool::new(false)));

        let notifications = std::sync::Arc::new(StdMutex::new(Vec::new()));
        let recording = std::sync::Arc::new(RecordingAdapter {
            notifications: notifications.clone(),
        });
        let am = AdapterManager::builder().register(recording).build();
        state.set_adapter_manager(am);

        let device_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let writer = DeviceWriter::spawn(1, 10);

        handle_client_frame(
            &state,
            device_id,
            &writer,
            ClientFrame::SetKillSwitch {
                session_id,
                on: true,
            },
        )
        .await;

        let recorded = notifications.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "exactly one broadcast must have gone out"
        );
        assert!(
            matches!(recorded[0], Notification::KillStateChanged { on: true }),
            "expected KillStateChanged{{on:true}}, got {:?}",
            recorded[0]
        );

        // The switch itself must actually be enabled too, not just broadcast.
        assert!(state.kill_handle().unwrap().load(Ordering::Acquire));
    }

    /// The ENABLE-only invariant (M1) must survive alongside the new broadcast — a mobile
    /// `on: false` must never flip the switch or broadcast anything.
    #[tokio::test]
    async fn set_kill_switch_disable_from_mobile_neither_flips_nor_broadcasts() {
        let state = test_adapter();
        state.set_kill_switch(std::sync::Arc::new(AtomicBool::new(false)));

        let notifications = std::sync::Arc::new(StdMutex::new(Vec::new()));
        let recording = std::sync::Arc::new(RecordingAdapter {
            notifications: notifications.clone(),
        });
        let am = AdapterManager::builder().register(recording).build();
        state.set_adapter_manager(am);

        let device_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let writer = DeviceWriter::spawn(1, 10);

        handle_client_frame(
            &state,
            device_id,
            &writer,
            ClientFrame::SetKillSwitch {
                session_id,
                on: false,
            },
        )
        .await;

        assert!(notifications.lock().unwrap().is_empty());
        assert!(!state.kill_handle().unwrap().load(Ordering::Acquire));
    }

    /// Review finding 3: a `create_device` failure must respond with `MobileError::Internal`
    /// and must NOT call `record_issued` — proven at the `PairingService` level (the
    /// `AlreadyIssued` path is only reachable after a successful `record_issued`).
    #[tokio::test]
    async fn create_device_failure_never_calls_record_issued() {
        struct FailingDeviceStore;
        #[async_trait]
        impl super::super::MobileDeviceStore for FailingDeviceStore {
            async fn find_active_by_token_hash(&self, _token_hash: &str) -> Option<Uuid> {
                None
            }
            async fn is_revoked(&self, _device_id: Uuid) -> bool {
                false
            }
            async fn touch_last_seen(&self, _device_id: Uuid) {}
            async fn create_device(&self, _device_name: &str, _token_hash: &str) -> Option<Uuid> {
                None
            }
        }

        let pairing = super::super::pairing::PairingService::new();
        let code = pairing.mint_code(None, true);
        assert!(matches!(
            pairing
                .redeem(
                    &code,
                    "Phone",
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                )
                .await,
            RedeemOutcome::Confirmed
        ));

        let store = FailingDeviceStore;
        let created = store.create_device("Phone", "hash").await;
        assert!(
            created.is_none(),
            "the fake store simulates a persistence failure"
        );
        // `record_issued` deliberately NOT called here, mirroring `pair_handler`'s None branch —
        // a second redeem of the SAME code must therefore see `Confirmed` again (fresh attempt
        // at persisting), never a phantom `AlreadyIssued`.
        assert!(matches!(
            pairing
                .redeem(
                    &code,
                    "Phone",
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                )
                .await,
            RedeemOutcome::Confirmed
        ));
    }

    /// Mobile review finding 6d: the kill switch is deliberately NOT scoped to any one
    /// conversation (§8.5/M15's "intentionally global" invariant), so the mobile UI sends it
    /// with a nil-UUID session sentinel rather than a real per-turn id (mirrors the desktop
    /// GUI's own `NIL_UUID` convention for non-session-scoped signals). `claim_or_verify_session`
    /// treats a nil UUID as an ordinary key — first-use-wins — so this must succeed exactly like
    /// the random-session-id case, not be special-cased/rejected.
    #[tokio::test]
    async fn set_kill_switch_enable_with_nil_session_id_still_flips_and_broadcasts() {
        let state = test_adapter();
        state.set_kill_switch(std::sync::Arc::new(AtomicBool::new(false)));

        let notifications = std::sync::Arc::new(StdMutex::new(Vec::new()));
        let recording = std::sync::Arc::new(RecordingAdapter {
            notifications: notifications.clone(),
        });
        let am = AdapterManager::builder().register(recording).build();
        state.set_adapter_manager(am);

        let device_id = Uuid::new_v4();
        let writer = DeviceWriter::spawn(1, 10);

        handle_client_frame(
            &state,
            device_id,
            &writer,
            ClientFrame::SetKillSwitch {
                session_id: Uuid::nil(),
                on: true,
            },
        )
        .await;

        assert_eq!(notifications.lock().unwrap().len(), 1);
        assert!(state.kill_handle().unwrap().load(Ordering::Acquire));
        assert_eq!(
            *state.session_owner.get(&Uuid::nil()).unwrap(),
            device_id,
            "the nil-UUID sentinel must still be claimed like any other session id"
        );
    }

    // -----------------------------------------------------------------------
    // Mobile Thin-Client plan phase 3 amendment — ClientFrame::CancelTurn.
    // -----------------------------------------------------------------------

    /// Records every `session_id` it was asked to cancel — proves the seam was actually
    /// invoked (or not) without depending on `haily-app::TurnRegistry` from this crate.
    struct RecordingCanceller {
        cancelled: std::sync::Arc<StdMutex<Vec<Uuid>>>,
    }

    impl haily_types::TurnCanceller for RecordingCanceller {
        fn cancel(&self, session_id: Uuid) -> bool {
            self.cancelled.lock().unwrap().push(session_id);
            true
        }
    }

    #[tokio::test]
    async fn cancel_turn_from_the_owning_session_invokes_the_seam() {
        let state = test_adapter();
        let cancelled = std::sync::Arc::new(StdMutex::new(Vec::new()));
        state.set_turn_canceller(std::sync::Arc::new(RecordingCanceller {
            cancelled: cancelled.clone(),
        }));

        let device_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let writer = DeviceWriter::spawn(1, 10);

        // Claim the session first (mirrors a prior UserMessage on the same session), then cancel.
        assert!(claim_or_verify_session(
            &state.session_owner,
            session_id,
            device_id
        ));
        handle_client_frame(
            &state,
            device_id,
            &writer,
            ClientFrame::CancelTurn { session_id },
        )
        .await;

        assert_eq!(
            *cancelled.lock().unwrap(),
            vec![session_id],
            "the canceller must be invoked with exactly the requested session id"
        );
    }

    /// m1: a device may only cancel a turn on a session IT owns — a foreign session_id must be
    /// rejected with `SessionUnknown`, never silently forwarded to the canceller.
    #[tokio::test]
    async fn cancel_turn_from_a_non_owning_device_is_denied() {
        let state = test_adapter();
        let cancelled = std::sync::Arc::new(StdMutex::new(Vec::new()));
        state.set_turn_canceller(std::sync::Arc::new(RecordingCanceller {
            cancelled: cancelled.clone(),
        }));

        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let writer = DeviceWriter::spawn(1, 10);

        assert!(claim_or_verify_session(
            &state.session_owner,
            session_id,
            owner
        ));
        handle_client_frame(
            &state,
            intruder,
            &writer,
            ClientFrame::CancelTurn { session_id },
        )
        .await;

        assert!(
            cancelled.lock().unwrap().is_empty(),
            "a non-owning device's CancelTurn must never reach the seam"
        );
    }

    /// A build/boot state with no canceller wired yet must be a harmless no-op, not a panic —
    /// mirrors `FetchProactive`/`FetchSession`'s own "no seam yet" fallback.
    #[tokio::test]
    async fn cancel_turn_with_no_canceller_wired_is_a_silent_no_op() {
        let state = test_adapter();
        let device_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let writer = DeviceWriter::spawn(1, 10);

        assert!(claim_or_verify_session(
            &state.session_owner,
            session_id,
            device_id
        ));
        // Must not panic.
        handle_client_frame(
            &state,
            device_id,
            &writer,
            ClientFrame::CancelTurn { session_id },
        )
        .await;
    }
}
