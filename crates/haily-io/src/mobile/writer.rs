//! Per-device single-writer task (red team M9) — the ONE place `seq` is ever assigned for a
//! device, and the only place its ring buffer is ever mutated.
//!
//! `deliver`/`deliver_run_event`/`notify` all funnel through [`DeviceWriter::push`], which sends
//! into an UNBOUNDED channel and returns immediately — this is what makes those calls
//! non-blocking even when a device's socket is slow or absent (a flaky mobile link must never
//! stall the desktop runner or the daemon-wide `notify_all` fan-out). The bounded resource is
//! the ring buffer INSIDE the task (drop-oldest on overflow), not the inter-task channel.
use crate::mobile::ring_buffer::{RingBuffer, WindowExceeded};
use async_trait::async_trait;
use haily_types::{ServerBody, ServerFrame};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// How long the writer task waits for one frame to reach a live socket before giving up on
/// that socket and falling back to ring-buffer-only (offline) mode. Bounds the worst case a
/// single stalled write can hold up this device's OWN queue — it can never hold up any OTHER
/// device or the callers of `push` (those never wait on the socket at all).
const SOCKET_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Abstraction over "a live outbound half of this device's WebSocket" — lets the writer task
/// and its resume/attach protocol be unit-tested with an in-memory fake instead of a real axum
/// `WebSocket` split sink.
#[async_trait]
pub trait FrameSink: Send {
    async fn send_frame(&mut self, frame: &ServerFrame) -> bool;
}

pub enum ResumeOutcome {
    Ok,
    WindowExceeded,
}

enum WriterMsg {
    Body(ServerBody),
    /// A (re)connect: attach `sink` for future live delivery, replaying anything buffered since
    /// `last_seq` through the SAME sink first — serialized through this one task so replay can
    /// never race a concurrently-arriving live frame for the same reconnect.
    Resume {
        last_seq: Option<u64>,
        sink: Box<dyn FrameSink>,
        reply: oneshot::Sender<ResumeOutcome>,
    },
    Detach,
}

/// Handle callers hold to push frames or attach a socket. Cloneable — cheap (one `mpsc::Sender`).
///
/// DESIGN DECISION (review finding 6b): the inter-task channel (`tx`) is deliberately
/// UNBOUNDED, not capped at `ring_buffer_capacity`. Bounding it would reintroduce exactly the
/// M9 failure mode this module exists to prevent: a bounded channel's `send` blocks once full,
/// and the ONLY thing that drains this channel (the task in `run`, below) can itself be stalled
/// for up to `SOCKET_SEND_TIMEOUT` (5s) mid-drain on a slow socket write — so a bounded channel
/// would make `deliver`/`deliver_run_event`/`notify` block on THAT same 5s stall, the opposite
/// of "never blocks the desktop runner". The actually-bounded resource is the ring buffer INSIDE
/// the task (drop-oldest at `ring_buffer_capacity`), which caps RETAINED memory; the channel
/// only holds messages for the brief window between being pushed and being processed, which is
/// O(1) per message except during that 5s socket stall. A device that is disconnected AND
/// bombarded with an unbounded flood of pushes could in principle grow this channel unbounded —
/// judged an acceptable trade for a personal-scale, single-operator deployment; revisit if
/// profiling ever shows this queue growing large in practice.
#[derive(Clone)]
pub struct DeviceWriter {
    tx: mpsc::UnboundedSender<WriterMsg>,
}

impl DeviceWriter {
    /// Spawn the task and return its handle. `epoch` is the server's per-boot nonce (C4);
    /// `capacity` bounds the ring buffer (M8/M9 drop-oldest).
    pub fn spawn(epoch: u64, capacity: usize) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(rx, RingBuffer::new(epoch, capacity)));
        Self { tx }
    }

    /// Push a body to be buffered (and, if a socket is attached, forwarded live). Never blocks
    /// and never fails loudly — an unbounded send only errs if the task itself has ended
    /// (device fully torn down), which is a benign "nothing left to deliver to" case.
    pub fn push(&self, body: ServerBody) {
        let _ = self.tx.send(WriterMsg::Body(body));
    }

    /// Attach a freshly-connected socket, replaying anything buffered since `last_seq` through
    /// it first. Awaits the task's reply so the caller (the connection handler) knows whether to
    /// ALSO surface `MobileError::ResumeWindowExceeded` to the client.
    pub async fn resume(
        &self,
        last_seq: Option<u64>,
        sink: Box<dyn FrameSink>,
    ) -> anyhow::Result<ResumeOutcome> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriterMsg::Resume {
                last_seq,
                sink,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("device writer task has ended"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("device writer task dropped the resume reply"))
    }

    /// Detach the current socket (e.g. the connection handler's read loop ended) without tearing
    /// down the task — the ring buffer keeps accumulating so a later reconnect can resume.
    pub fn detach(&self) {
        let _ = self.tx.send(WriterMsg::Detach);
    }
}

async fn run(mut rx: mpsc::UnboundedReceiver<WriterMsg>, mut ring: RingBuffer) {
    let mut socket: Option<Box<dyn FrameSink>> = None;

    while let Some(msg) = rx.recv().await {
        match msg {
            WriterMsg::Body(body) => {
                let frame = ring.push(body);
                if let Some(sink) = socket.as_mut() {
                    if !send_with_timeout(sink.as_mut(), &frame).await {
                        socket = None; // dead link — ring buffer covers replay on reconnect
                    }
                }
            }
            WriterMsg::Resume {
                last_seq,
                mut sink,
                reply,
            } => {
                let outcome = match ring.replay_since(last_seq) {
                    Ok(frames) => {
                        for frame in &frames {
                            // Best-effort during replay: a failure here means the brand-new
                            // socket is already dead, which the connection handler's own
                            // read/write loop will discover independently.
                            let _ = send_with_timeout(sink.as_mut(), frame).await;
                        }
                        ResumeOutcome::Ok
                    }
                    Err(WindowExceeded) => ResumeOutcome::WindowExceeded,
                };
                socket = Some(sink);
                let _ = reply.send(outcome);
            }
            WriterMsg::Detach => socket = None,
        }
    }
}

async fn send_with_timeout(sink: &mut dyn FrameSink, frame: &ServerFrame) -> bool {
    match tokio::time::timeout(SOCKET_SEND_TIMEOUT, sink.send_frame(frame)).await {
        Ok(ok) => ok,
        Err(_) => {
            tracing::warn!(
                epoch = frame.epoch,
                seq = frame.seq,
                "mobile: socket send timed out — treating link as dead"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;

    /// In-memory sink recording every frame it receives — the writer task's protocol proof
    /// without a real WebSocket.
    struct RecordingSink {
        seqs: Arc<Mutex<Vec<u64>>>,
        notify: Arc<Notify>,
    }

    #[async_trait]
    impl FrameSink for RecordingSink {
        async fn send_frame(&mut self, frame: &ServerFrame) -> bool {
            self.seqs.lock().unwrap().push(frame.seq);
            self.notify.notify_one();
            true
        }
    }

    type RecordingSinkParts = (Box<dyn FrameSink>, Arc<Mutex<Vec<u64>>>, Arc<Notify>);

    fn recording_sink() -> RecordingSinkParts {
        let seqs = Arc::new(Mutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        (
            Box::new(RecordingSink {
                seqs: seqs.clone(),
                notify: notify.clone(),
            }),
            seqs,
            notify,
        )
    }

    #[tokio::test]
    async fn push_before_any_socket_is_attached_is_buffered_not_lost() {
        let writer = DeviceWriter::spawn(1, 100);
        writer.push(ServerBody::Pong);
        writer.push(ServerBody::Pong);

        let (sink, seqs, notify) = recording_sink();
        let outcome = writer.resume(None, sink).await.expect("resume");
        assert!(matches!(outcome, ResumeOutcome::Ok));
        notify.notified().await;
        // Give the task a beat to drain both replayed frames onto the sink.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(*seqs.lock().unwrap(), vec![0, 1]);
    }

    /// M9's core proof: pushes racing from multiple "callers" (simulating deliver /
    /// deliver_run_event / notify firing concurrently) still produce a strictly increasing,
    /// gap-free seq sequence — because they all funnel through the one task.
    #[tokio::test]
    async fn concurrent_pushes_produce_gap_free_strictly_increasing_seq() {
        let writer = DeviceWriter::spawn(7, 1000);
        let mut handles = Vec::new();
        for _ in 0..4 {
            let w = writer.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..25 {
                    w.push(ServerBody::Pong);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let (sink, seqs, _notify) = recording_sink();
        writer.resume(None, sink).await.expect("resume");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut got = seqs.lock().unwrap().clone();
        got.sort_unstable();
        let expected: Vec<u64> = (0..100).collect();
        assert_eq!(
            got, expected,
            "100 pushes across 4 tasks must yield seq 0..100 with no gaps/dupes"
        );
    }

    #[tokio::test]
    async fn live_push_after_resume_is_forwarded_to_the_attached_socket() {
        let writer = DeviceWriter::spawn(1, 100);
        let (sink, seqs, notify) = recording_sink();
        writer.resume(None, sink).await.expect("resume");

        writer.push(ServerBody::Pong);
        notify.notified().await;
        assert_eq!(*seqs.lock().unwrap(), vec![0]);
    }

    #[tokio::test]
    async fn detach_stops_live_forwarding_but_keeps_buffering() {
        let writer = DeviceWriter::spawn(1, 100);
        let (sink, seqs, _notify) = recording_sink();
        writer.resume(None, sink).await.expect("resume");
        writer.detach();

        writer.push(ServerBody::Pong);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            seqs.lock().unwrap().is_empty(),
            "detached socket must not receive live frames"
        );

        // But a later resume still sees the buffered frame.
        let (sink2, seqs2, _notify2) = recording_sink();
        writer.resume(None, sink2).await.expect("resume");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(*seqs2.lock().unwrap(), vec![0]);
    }
}
