//! Auth, resume, epoch-restart, overflow-recovery, session-scoping, and bind-failure E2E tests
//! (red team C4, M7, M8, M9's connection-loop half, M11, m1, m3).
use crate::support::{
    claim_session, connect_and_handshake, connect_ws, connect_ws_no_auth, expect_epoch,
    expect_kill_on, fence, hash_token, recv_frame_timeout, send_frame, send_hello,
    start_test_server, FakeSessionTranscript, DEFAULT_TIMEOUT,
};
use haily_io::mobile::MobileAdapter;
use haily_io::{Adapter, ResponseChunk, TranscriptEntry};
use haily_types::{ClientFrame, DepthMode, MobileError, ServerBody};
use std::net::TcpListener as StdTcpListener;
use uuid::Uuid;

#[tokio::test]
async fn missing_authorization_header_is_rejected_before_upgrade() {
    let server = start_test_server(|_| {}).await;
    let err = connect_ws_no_auth(server.port)
        .await
        .expect_err("no header must fail the upgrade");
    assert_http_status(&err, 401);
}

#[tokio::test]
async fn unknown_token_is_rejected_before_upgrade() {
    let server = start_test_server(|_| {}).await;
    let err = connect_ws(server.port, "never-registered-token")
        .await
        .expect_err("an unregistered token must fail the upgrade");
    assert_http_status(&err, 401);
}

#[tokio::test]
async fn revoked_token_is_rejected_before_upgrade() {
    let server = start_test_server(|_| {}).await;
    let token = "revoked-token";
    let device_id = server.devices.register(&hash_token(token));
    server.devices.revoke(device_id);

    let err = connect_ws(server.port, token)
        .await
        .expect_err("a revoked device's token must fail the upgrade");
    assert_http_status(&err, 401);
}

fn assert_http_status(err: &tokio_tungstenite::tungstenite::Error, expected: u16) {
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status().as_u16(), expected);
        }
        other => panic!("expected an HTTP {expected} rejection, got {other:?}"),
    }
}

/// m3: revoking an already-connected device must close its LIVE socket immediately — not just
/// reject its next reconnect attempt. `disconnect_device` is the documented seam a future
/// Devices-panel "Revoke" button calls (already wired end-to-end at the app layer in
/// `haily-app::mobile_admin::revoke_device`); invoking it directly here is the same call a real
/// revoke performs, exercised over a REAL live connection rather than the unit-level
/// `mod::tests::disconnect_device_*` checks.
#[tokio::test]
async fn revoking_a_connected_device_closes_its_live_socket() {
    let server = start_test_server(|_| {}).await;
    let token = "mid-session-token";
    let device_id = server.devices.register(&hash_token(token));

    let (mut ws, _frames) = connect_and_handshake(server.port, token, None, None).await;

    server.adapter.disconnect_device(device_id);

    let next = recv_frame_timeout(&mut ws, DEFAULT_TIMEOUT).await;
    assert!(
        next.is_none(),
        "the connection must close (stream end / no further frame), got {next:?}"
    );
}

/// The core resume contract: frames missed while disconnected replay exactly once, then live
/// delivery continues — no gap, no duplicate of what was already seen.
#[tokio::test]
async fn resume_replays_missed_frames_exactly_once_then_continues_live() {
    let server = start_test_server(|_| {}).await;
    let token = "resume-token";
    server.devices.register(&hash_token(token));
    let session_id = Uuid::new_v4();

    let (mut ws, frames) = connect_and_handshake(server.port, token, None, None).await;
    let epoch = expect_epoch(frames.last().unwrap());
    claim_session(&mut ws, session_id).await;

    for i in 0..5u32 {
        server
            .adapter
            .deliver(session_id, ResponseChunk::Text(format!("live-{i}")))
            .await
            .unwrap();
    }
    let mut last_seen = 0u64;
    for _ in 0..5 {
        let frame = recv_frame_timeout(&mut ws, DEFAULT_TIMEOUT)
            .await
            .expect("a live chunk must arrive");
        last_seen = frame.seq;
    }
    drop(ws); // simulate the phone going offline

    for i in 5..8u32 {
        server
            .adapter
            .deliver(session_id, ResponseChunk::Text(format!("buffered-{i}")))
            .await
            .unwrap();
    }

    let (mut ws2, replay) =
        connect_and_handshake(server.port, token, Some(last_seen), Some(epoch)).await;
    // Replay must be exactly the 3 missed chunks, PLUS this reconnect's own fresh HelloAck
    // (also pushed into the same per-device ring buffer — every frame type shares the seq
    // space) — no duplicates of the first 5 already-seen live chunks.
    let replayed_texts: Vec<String> = replay
        .iter()
        .filter_map(|f| match &f.body {
            ServerBody::Chunk {
                chunk: ResponseChunk::Text(t),
                ..
            } => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        replayed_texts,
        vec!["buffered-5", "buffered-6", "buffered-7"],
        "replay must be exactly the missed frames, in order, no duplicates of the first 5"
    );
    let seqs: Vec<u64> = replay.iter().map(|f| f.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "replayed seqs must be strictly increasing");
    assert!(
        matches!(replay.last().unwrap().body, ServerBody::HelloAck { .. }),
        "the reconnect's own HelloAck must be the newest entry in the replay"
    );

    server
        .adapter
        .deliver(session_id, ResponseChunk::Text("after-resume".into()))
        .await
        .unwrap();
    let live = recv_frame_timeout(&mut ws2, DEFAULT_TIMEOUT)
        .await
        .expect("live delivery must continue after resume");
    assert!(matches!(
        live.body,
        ServerBody::Chunk { chunk: ResponseChunk::Text(t), .. } if t == "after-resume"
    ));
}

/// C4: a restarted server has a different `epoch` and an empty seq space. A reconnecting client
/// presenting its OLD epoch must be forced into a full resync rather than have its stale cursor
/// silently swallow every live frame (the exact failure mode C4 exists to prevent) — and, once
/// resynced, live delivery must keep working, proving the client is not permanently stalled.
///
/// A genuine process restart cannot be simulated by rebinding the SAME port from within one test
/// process (the original listener has no shutdown API — `server.rs` never exposes one, by
/// design, since production restarts via a fresh process). A second `MobileAdapter` instance —
/// fresh epoch, empty ring buffers/session claims, same shared device store, bound to a NEW
/// ephemeral port — is a faithful proxy: the server-side epoch-comparison logic under test
/// (`connection_loop`'s `cursor = if last_epoch == Some(state.epoch) { .. } else { None }`)
/// never inspects the socket address, only the `Hello` payload against its own `state.epoch`.
#[tokio::test]
async fn epoch_restart_forces_full_resync_and_live_frames_continue() {
    let first = start_test_server(|_| {}).await;
    let token = "restart-token";
    first.devices.register(&hash_token(token));
    let session_id = Uuid::new_v4();

    let (mut ws, frames) = connect_and_handshake(first.port, token, None, None).await;
    let old_epoch = expect_epoch(frames.last().unwrap());
    claim_session(&mut ws, session_id).await;
    first
        .adapter
        .deliver(session_id, ResponseChunk::Text("before-restart".into()))
        .await
        .unwrap();
    let before = recv_frame_timeout(&mut ws, DEFAULT_TIMEOUT).await.unwrap();
    let old_cursor = before.seq;
    drop(ws);

    // "Restart": a fresh adapter instance sharing the SAME device store (same persisted
    // devices table surviving a real restart) but with all in-memory state (epoch, ring
    // buffers, session claims) wiped, per a real process boot.
    let second_port = crate::support::reserve_ephemeral_port();
    let second_config = haily_io::mobile::MobileServerConfig {
        enabled: true,
        port: second_port,
        lan_opt_in: false,
        ..haily_io::mobile::MobileServerConfig::default()
    };
    let second_adapter =
        MobileAdapter::new(second_config, first.devices.clone(), std::env::temp_dir());
    let (tx2, mut rx2) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move { while rx2.recv().await.is_some() {} });
    assert!(second_adapter.start_and_await_bind(tx2).await);

    let (mut ws2, frames2) =
        connect_and_handshake(second_port, token, Some(old_cursor), Some(old_epoch)).await;
    let new_epoch = expect_epoch(frames2.last().unwrap());
    assert_ne!(new_epoch, old_epoch, "a restart must mint a fresh epoch");

    // No stale/duplicate replay from the old epoch's seq space — the client must re-claim
    // (TOFU) and fetch a snapshot rather than expect an implicit carryover.
    send_frame(&mut ws2, ClientFrame::FetchSession { session_id }).await;
    let snapshot_frame = recv_frame_timeout(&mut ws2, DEFAULT_TIMEOUT)
        .await
        .expect("FetchSession must be answered after an epoch mismatch, not stall");
    assert!(
        matches!(snapshot_frame.body, ServerBody::SessionSnapshot(_)),
        "expected SessionSnapshot, got {:?}",
        snapshot_frame.body
    );

    // Live delivery on the NEW adapter instance must keep working — the "does not stall" proof.
    second_adapter
        .deliver(session_id, ResponseChunk::Text("after-restart".into()))
        .await
        .unwrap();
    let live = recv_frame_timeout(&mut ws2, DEFAULT_TIMEOUT)
        .await
        .expect("live delivery must continue post-resync");
    assert!(matches!(
        live.body,
        ServerBody::Chunk { chunk: ResponseChunk::Text(t), .. } if t == "after-restart"
    ));
}

/// M7: once the ring buffer's window is exceeded, the client must recover via
/// `FetchSession`/`SessionSnapshot` and end up CONSISTENT — not merely receive the error.
///
/// This reconnect does NOT use `connect_and_handshake`: a resume-window-exceeded reply short-
/// circuits the replay entirely (see `writer.rs::run`'s `Err(WindowExceeded)` arm), so no
/// `HelloAck` is ever delivered on THIS connection — only the `Error(ResumeWindowExceeded)`
/// push that follows. Connecting/reading manually here reflects that real wire behavior instead
/// of assuming a `HelloAck` that never arrives.
#[tokio::test]
async fn resume_window_exceeded_recovers_via_fetch_session_snapshot() {
    let transcript = FakeSessionTranscript::new();
    let server = start_test_server(|cfg| cfg.ring_buffer_capacity = 3).await;
    server.adapter.set_session_transcript(transcript.clone());
    let token = "overflow-token";
    server.devices.register(&hash_token(token));
    let session_id = Uuid::new_v4();
    transcript.seed(
        session_id,
        vec![TranscriptEntry {
            role: "user".into(),
            content: "hi".into(),
        }],
    );

    let (mut ws, frames) = connect_and_handshake(server.port, token, None, None).await;
    let epoch = expect_epoch(frames.last().unwrap());
    let cursor = frames.last().unwrap().seq; // 0 — nothing else has been sent yet
    claim_session(&mut ws, session_id).await;
    drop(ws); // go offline before the overflow-inducing burst

    // Push well past the capacity (3) so the old cursor (0) predates every retained entry.
    for i in 0..6u32 {
        server
            .adapter
            .deliver(session_id, ResponseChunk::Text(format!("burst-{i}")))
            .await
            .unwrap();
    }

    let mut ws2 = connect_ws(server.port, token)
        .await
        .expect("reconnect upgrade must succeed");
    send_hello(&mut ws2, Some(cursor), Some(epoch)).await;
    let error_frame = recv_frame_timeout(&mut ws2, DEFAULT_TIMEOUT)
        .await
        .expect("a ResumeWindowExceeded error must arrive");
    assert!(matches!(
        error_frame.body,
        ServerBody::Error(MobileError::ResumeWindowExceeded)
    ));

    send_frame(&mut ws2, ClientFrame::FetchSession { session_id }).await;
    let snapshot_frame = recv_frame_timeout(&mut ws2, DEFAULT_TIMEOUT)
        .await
        .expect("SessionSnapshot must arrive");
    match snapshot_frame.body {
        ServerBody::SessionSnapshot(snapshot) => {
            assert_eq!(snapshot.session_id, session_id);
            assert_eq!(snapshot.transcript.len(), 1);
            assert_eq!(snapshot.transcript[0].content, "hi");
        }
        other => panic!("expected SessionSnapshot, got {other:?}"),
    }

    // Consistency proof: live delivery works normally after the recovery, not just the snapshot.
    server
        .adapter
        .deliver(session_id, ResponseChunk::Text("post-recovery".into()))
        .await
        .unwrap();
    let live = recv_frame_timeout(&mut ws2, DEFAULT_TIMEOUT)
        .await
        .expect("live delivery must resume after recovery");
    assert!(matches!(
        live.body,
        ServerBody::Chunk { chunk: ResponseChunk::Text(t), .. } if t == "post-recovery"
    ));
}

/// m1: a session claimed by one device must reject a session-scoped frame from ANY other
/// device, over the full wire path (two real, distinct WS connections).
#[tokio::test]
async fn foreign_device_session_scoped_frame_is_rejected() {
    let server = start_test_server(|_| {}).await;
    let owner_token = "owner-token";
    let intruder_token = "intruder-token";
    server.devices.register(&hash_token(owner_token));
    server.devices.register(&hash_token(intruder_token));
    let session_id = Uuid::new_v4();

    let (mut owner_ws, _) = connect_and_handshake(server.port, owner_token, None, None).await;
    claim_session(&mut owner_ws, session_id).await;

    let (mut intruder_ws, _) = connect_and_handshake(server.port, intruder_token, None, None).await;
    send_frame(&mut intruder_ws, ClientFrame::FetchProactive { session_id }).await;
    let rejection = recv_frame_timeout(&mut intruder_ws, DEFAULT_TIMEOUT)
        .await
        .expect("a rejection must arrive");
    assert!(matches!(
        rejection.body,
        ServerBody::Error(MobileError::SessionUnknown)
    ));

    // The owner's OWN use of the same session must still work.
    send_frame(&mut owner_ws, ClientFrame::FetchProactive { session_id }).await;
    let ok = recv_frame_timeout(&mut owner_ws, DEFAULT_TIMEOUT)
        .await
        .expect("the owner's own FetchProactive must succeed");
    assert!(matches!(ok.body, ServerBody::ProactiveList(_)));
}

/// M11: a port already in use must degrade the mobile server, never abort the caller.
///
/// Occupies EVERY address `select_bind_addrs` would itself select for this port — not just
/// loopback — so the test is correct regardless of the host's own network configuration (a dev
/// machine on a real tailnet, like this one, also has a CGNAT-range interface `select_bind_addrs`
/// unconditionally includes; occupying loopback alone left that address free to bind
/// successfully, which made the adapter genuinely NOT degraded and produced a false failure).
#[tokio::test]
async fn bind_failure_on_an_occupied_port_degrades_instead_of_aborting() {
    let occupied_port = crate::support::reserve_ephemeral_port();
    let interfaces = haily_io::mobile::bind::enumerate_interfaces();
    let candidate_addrs =
        haily_io::mobile::bind::select_bind_addrs(&interfaces, false, occupied_port);
    // Keep every listener alive for the whole test so all candidate addresses stay occupied.
    let _occupiers: Vec<StdTcpListener> = candidate_addrs
        .iter()
        .map(|addr| StdTcpListener::bind(addr).expect("occupy every candidate bind address"))
        .collect();

    let devices = crate::support::FakeDeviceStore::new();
    let config = haily_io::mobile::MobileServerConfig {
        enabled: true,
        port: occupied_port,
        lan_opt_in: false,
        ..haily_io::mobile::MobileServerConfig::default()
    };
    let adapter = MobileAdapter::new(config, devices, std::env::temp_dir());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    // The fire-and-forget path must still report Ok — binding happens in the spawned task.
    assert!(adapter.start(tx.clone()).await.is_ok());

    // The awaiting path proves the bind genuinely failed (degraded), not merely unobserved.
    let bound = adapter.start_and_await_bind(tx).await;
    assert!(
        !bound,
        "binding every already-occupied candidate address must report false (degraded), not panic"
    );
}

/// M1/M15 over the full wire: mobile can enable the (global) kill switch, the new state
/// survives reconnect (observable via the next `HelloAck.kill_on`), and mobile can never
/// disable it remotely. Each reconnect passes its own precise cursor/epoch (via `fence`'s
/// returned seq) so the replay is exactly the fresh `HelloAck`, never a full-history replay from
/// the very first connection (which would otherwise show the STALE pre-toggle `kill_on`).
#[tokio::test]
async fn kill_switch_enable_only_persists_and_cannot_be_disabled_remotely() {
    let server = start_test_server(|_| {}).await;
    server
        .adapter
        .set_kill_switch(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )));
    let token = "kill-switch-token";
    server.devices.register(&hash_token(token));
    let session_id = Uuid::new_v4();

    let (mut ws, frames) = connect_and_handshake(server.port, token, None, None).await;
    let epoch = expect_epoch(frames.last().unwrap());
    assert!(!expect_kill_on(frames.last().unwrap()));

    send_frame(
        &mut ws,
        ClientFrame::SetKillSwitch {
            session_id,
            on: true,
        },
    )
    .await;
    let cursor = fence(&mut ws).await;
    drop(ws);

    let (mut ws2, frames2) =
        connect_and_handshake(server.port, token, Some(cursor), Some(epoch)).await;
    assert!(
        expect_kill_on(frames2.last().unwrap()),
        "the enable must persist and be visible on the next HelloAck"
    );

    send_frame(
        &mut ws2,
        ClientFrame::SetKillSwitch {
            session_id,
            on: false,
        },
    )
    .await;
    let cursor2 = fence(&mut ws2).await;
    drop(ws2);

    let (_ws3, frames3) =
        connect_and_handshake(server.port, token, Some(cursor2), Some(epoch)).await;
    assert!(
        expect_kill_on(frames3.last().unwrap()),
        "mobile must never be able to disable the kill switch remotely"
    );
}

/// m2: a mobile client requesting `DepthMode::Deep` (3–5x cost) must be silently downgraded to
/// `Normal` before the request ever reaches the orchestrator — `deny_remote_deep` defaults to
/// `true` (`MobileServerConfig::default`), so `start_test_server`'s default config already
/// exercises the enforcing branch; this asserts the FORWARDED `Request` (not just the client's
/// own belief about what it asked for) actually carries `Normal`.
#[tokio::test]
async fn remote_deep_depth_is_downgraded_to_normal_before_forwarding() {
    let server = start_test_server(|_| {}).await;
    let token = "deep-depth-token";
    server.devices.register(&hash_token(token));
    let session_id = Uuid::new_v4();

    let (mut ws, _frames) = connect_and_handshake(server.port, token, None, None).await;
    send_frame(
        &mut ws,
        ClientFrame::UserMessage {
            session_id,
            message: "do something expensive".to_string(),
            depth: DepthMode::Deep,
        },
    )
    .await;
    fence(&mut ws).await;

    // `fence` only proves the SERVER's read loop processed the frame, not that the async
    // forward-to-orchestrator send (a separate spawned task draining the mpsc into the capture
    // Vec) has completed yet — poll with a bounded deadline instead of a blind sleep, so this
    // resolves the instant the push lands rather than after a fixed guess.
    let req = tokio::time::timeout(DEFAULT_TIMEOUT, async {
        loop {
            if let Some(r) = server
                .forwarded_requests
                .lock()
                .await
                .iter()
                .find(|r| r.session_id == session_id)
                .cloned()
            {
                return r;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the UserMessage must have been forwarded to the orchestrator");
    assert_eq!(
        req.depth,
        DepthMode::Normal,
        "deny_remote_deep must downgrade Deep to Normal on the request that actually reaches \
         the orchestrator, not just report success back to the client"
    );
}
