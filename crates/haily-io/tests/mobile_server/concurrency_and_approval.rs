//! M9 (single-writer, gap-free concurrent seq) and M10 (dead-approval reconcile) E2E tests.
use crate::support::{
    claim_session, connect_and_handshake, expect_epoch, fence, hash_token, recv_frame_timeout,
    send_frame, start_test_server, FakeApprovalResolver, DEFAULT_TIMEOUT,
};
use haily_io::{Adapter, ResponseChunk, RunEvent};
use haily_types::{ClientFrame, Notification, ServerBody};
use uuid::Uuid;

/// M9: `deliver`, `deliver_run_event`, and `notify(KillStateChanged)` are three independently-
/// firing callbacks that must all funnel through ONE serialization point per device connection.
/// Firing them concurrently from real `tokio::spawn` tasks (not just hammering the writer
/// directly, which `writer::tests` already covers) and reading the RESULT over the actual wire
/// proves the full adapter-level chokepoint — `push_for_session` + `notify`'s writer iteration —
/// doesn't reintroduce a race the lower-level unit test wouldn't see.
#[tokio::test]
async fn concurrent_deliver_paths_produce_gap_free_strictly_increasing_seq_on_the_wire() {
    let server = start_test_server(|_| {}).await;
    let token = "concurrency-token";
    server.devices.register(&hash_token(token));
    let session_id = Uuid::new_v4();

    let (mut ws, _frames) = connect_and_handshake(server.port, token, None, None).await;
    claim_session(&mut ws, session_id).await;

    const N: usize = 10;
    let mut handles = Vec::new();
    for i in 0..N {
        let adapter = server.adapter.clone();
        handles.push(tokio::spawn(async move {
            adapter
                .deliver(session_id, ResponseChunk::Text(format!("chunk-{i}")))
                .await
                .unwrap();
        }));
    }
    for i in 0..N {
        let adapter = server.adapter.clone();
        handles.push(tokio::spawn(async move {
            adapter
                .deliver_run_event(
                    session_id,
                    RunEvent::RunStarted {
                        run_id: format!("run-{i}"),
                        work_item_id: "wi".to_string(),
                    },
                )
                .await
                .unwrap();
        }));
    }
    for _ in 0..N {
        let adapter = server.adapter.clone();
        handles.push(tokio::spawn(async move {
            adapter
                .notify(Notification::KillStateChanged { on: true })
                .await
                .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let mut seqs = Vec::new();
    let mut chunks = 0;
    let mut runs = 0;
    let mut kills = 0;
    let total = 3 * N;
    for _ in 0..total {
        let frame = recv_frame_timeout(&mut ws, DEFAULT_TIMEOUT)
            .await
            .expect("every storm frame must arrive");
        seqs.push(frame.seq);
        match frame.body {
            ServerBody::Chunk { .. } => chunks += 1,
            ServerBody::Run { .. } => runs += 1,
            ServerBody::KillState { .. } => kills += 1,
            other => panic!("unexpected frame kind in the storm: {other:?}"),
        }
    }

    assert_eq!(
        chunks, N,
        "every deliver() call must have produced exactly one Chunk"
    );
    assert_eq!(
        runs, N,
        "every deliver_run_event() call must have produced exactly one Run"
    );
    assert_eq!(
        kills, N,
        "every notify() call must have produced exactly one KillState"
    );

    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        seqs.len(),
        sorted.len(),
        "no duplicate seq must appear across the concurrent storm"
    );
    assert_eq!(
        seqs,
        {
            let mut s = seqs.clone();
            s.sort_unstable();
            s
        },
        "seqs must arrive on the wire in strictly increasing order (single writer)"
    );
    // Gap-free: the storm's seqs must be one contiguous run (the claim/fence frames before it
    // consumed the lower seqs, so this run starts wherever those left off).
    let min = *seqs.iter().min().unwrap();
    let max = *seqs.iter().max().unwrap();
    assert_eq!(
        (max - min + 1) as usize,
        total,
        "seqs must be gap-free across the whole concurrent storm"
    );
}

/// M10: an `ApprovalNeeded` chunk delivered while the device is disconnected replays on
/// reconnect (P2a's documented gap, D6 — `docs/mobile-protocol.md` §8.6 / the phase's own
/// Deviation Log: replay suppression is NOT implemented) but a late `Approve` for it must still
/// resolve `false` with no effect, because the mobile adapter faithfully delegates to the
/// approval resolver rather than fabricating its own "still valid" answer. This test proves
/// exactly that delegation — NOT that the replayed frame is rendered inert (that half is the
/// known, reported gap, not something this phase fixes).
#[tokio::test]
async fn a_replayed_expired_approval_resolves_false_on_a_late_approve() {
    let resolver = FakeApprovalResolver::new();
    let server = start_test_server(|_| {}).await;
    server.adapter.set_approval_resolver(resolver.clone());
    let token = "dead-approval-token";
    server.devices.register(&hash_token(token));
    let session_id = Uuid::new_v4();
    let approval_id = Uuid::new_v4();
    // Deliberately NOT seeding `approval_id` as pending — models an approval the broker already
    // deny-on-timeout'd (120s) and evicted before this replay/Approve ever arrives.

    let (mut ws, frames) = connect_and_handshake(server.port, token, None, None).await;
    let epoch = expect_epoch(frames.last().unwrap());
    let cursor = claim_session(&mut ws, session_id).await;

    server
        .adapter
        .deliver(
            session_id,
            ResponseChunk::ToolApprovalRequest {
                tool: "task_delete".to_string(),
                args: "{}".to_string(),
                approval_id,
                origin: None,
                reversible: false,
            },
        )
        .await
        .unwrap();
    drop(ws); // the phone goes offline (backgrounded) before ever seeing this frame

    let (mut ws2, replay) =
        connect_and_handshake(server.port, token, Some(cursor), Some(epoch)).await;
    let approval_replayed = replay.iter().any(|f| {
        matches!(
            &f.body,
            ServerBody::Chunk {
                chunk: ResponseChunk::ToolApprovalRequest { approval_id: id, .. },
                ..
            } if *id == approval_id
        )
    });
    assert!(
        approval_replayed,
        "the stale ApprovalNeeded must have replayed (P2a's D6 gap — not fixed here)"
    );

    send_frame(
        &mut ws2,
        ClientFrame::Approve {
            approval_id,
            session_id,
            approved: true,
            biometric_ok: true,
        },
    )
    .await;
    fence(&mut ws2).await; // proves Approve was already processed

    let calls = resolver.calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        1,
        "resolve() must have been called exactly once"
    );
    let (id, sid, approved, returned) = calls[0];
    assert_eq!(id, approval_id);
    assert_eq!(sid, session_id);
    assert!(approved, "mobile forwarded the client's own approved=true");
    assert!(
        !returned,
        "the broker must reject a late resolve for an id it no longer considers pending"
    );
}

/// Contrast case for the test above: a STILL-LIVE approval (seeded as pending, the broker
/// hasn't timed it out) resolves `true` on an ordinary in-session `Approve` — proves
/// `FakeApprovalResolver` itself is a faithful stand-in (not merely hard-coded to always deny)
/// and that the mobile path's normal, non-expired approval flow works end to end.
#[tokio::test]
async fn a_live_pending_approval_resolves_true_on_approve() {
    let resolver = FakeApprovalResolver::new();
    let server = start_test_server(|_| {}).await;
    server.adapter.set_approval_resolver(resolver.clone());
    let token = "live-approval-token";
    server.devices.register(&hash_token(token));
    let session_id = Uuid::new_v4();
    let approval_id = Uuid::new_v4();
    resolver.seed_pending(approval_id);

    let (mut ws, _frames) = connect_and_handshake(server.port, token, None, None).await;
    claim_session(&mut ws, session_id).await;
    server
        .adapter
        .deliver(
            session_id,
            ResponseChunk::ToolApprovalRequest {
                tool: "task_delete".to_string(),
                args: "{}".to_string(),
                approval_id,
                origin: None,
                reversible: false,
            },
        )
        .await
        .unwrap();
    recv_frame_timeout(&mut ws, DEFAULT_TIMEOUT)
        .await
        .expect("the ToolApprovalRequest chunk must arrive live");

    send_frame(
        &mut ws,
        ClientFrame::Approve {
            approval_id,
            session_id,
            approved: true,
            biometric_ok: true,
        },
    )
    .await;
    fence(&mut ws).await;

    let calls = resolver.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let (id, sid, approved, returned) = calls[0];
    assert_eq!(id, approval_id);
    assert_eq!(sid, session_id);
    assert!(approved);
    assert!(returned, "a still-pending approval must resolve true");
}
