//! Per-device outbound frame ring buffer (red team C4/M8/M9).
//!
//! Owned exclusively by one device's writer task (`writer.rs`) — every `ServerFrame` the
//! server ever sends that device is assigned its `seq` here, in the SAME per-device counter
//! regardless of frame kind (`Chunk`/`Run`/`Notify`/`Pong`/`HelloAck`/…), matching the wire
//! contract in `docs/mobile-protocol.md` §2.2. The buffer persists across a device's socket
//! reconnects within one server boot (one `epoch`) — that persistence, not the raw TCP
//! connection, is what makes `Hello{last_seen_seq}` resume actually work.
use haily_types::{ServerBody, ServerFrame};
use std::collections::VecDeque;

/// The resume request predates everything the buffer still retains — the caller must fall
/// back to `FetchSession`/`SessionSnapshot` (red team M7) rather than replay a partial/gappy
/// history.
#[derive(Debug, PartialEq, Eq)]
pub struct WindowExceeded;

pub struct RingBuffer {
    epoch: u64,
    /// The `seq` that will be assigned to the NEXT pushed frame.
    next_seq: u64,
    capacity: usize,
    entries: VecDeque<ServerFrame>,
}

impl RingBuffer {
    pub fn new(epoch: u64, capacity: usize) -> Self {
        Self {
            epoch,
            next_seq: 0,
            capacity: capacity.max(1),
            entries: VecDeque::new(),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Assign the next `seq`, wrap `body` in a `ServerFrame`, and append it — dropping the
    /// oldest entry first if the buffer is already at capacity (red team M9: an intentional,
    /// documented deviation from the GUI adapter's never-drop contract, since a flaky mobile
    /// link must never stall the desktop runner and resume/`FetchSession` cover the gap).
    pub fn push(&mut self, body: ServerBody) -> ServerFrame {
        let frame = ServerFrame {
            epoch: self.epoch,
            seq: self.next_seq,
            body,
        };
        self.next_seq += 1;
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(frame.clone());
        frame
    }

    /// Frames with `seq > last_seq`, oldest first, for a `Hello`-driven resume within the SAME
    /// epoch (red team C4 — an epoch mismatch is handled entirely by the caller before this is
    /// ever invoked; this function has no epoch-comparison logic of its own).
    ///
    /// `last_seq: None` means "no prior cursor" (first-ever connect for this device) — returns
    /// everything currently retained. `Err(WindowExceeded)` when `last_seq` predates the oldest
    /// retained entry (or predates entries dropped before the buffer was ever this empty) — the
    /// gap is real, not just "nothing new happened yet".
    pub fn replay_since(&self, last_seq: Option<u64>) -> Result<Vec<ServerFrame>, WindowExceeded> {
        let Some(cursor) = last_seq else {
            return Ok(self.entries.iter().cloned().collect());
        };
        // The oldest seq still coverable: whatever the buffer currently retains, or — if it is
        // empty — the next seq that would be assigned (nothing before that point was ever
        // dropped without being retained, so an empty-and-fresh buffer is not itself a gap).
        let oldest_available = self.entries.front().map(|f| f.seq).unwrap_or(self.next_seq);
        if cursor + 1 < oldest_available {
            return Err(WindowExceeded);
        }
        Ok(self
            .entries
            .iter()
            .filter(|f| f.seq > cursor)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_types::ServerBody;

    fn body() -> ServerBody {
        ServerBody::Pong
    }

    #[test]
    fn seq_is_gap_free_and_strictly_increasing() {
        let mut ring = RingBuffer::new(1, 100);
        let mut seqs = Vec::new();
        for _ in 0..10 {
            seqs.push(ring.push(body()).seq);
        }
        let expected: Vec<u64> = (0..10).collect();
        assert_eq!(seqs, expected);
    }

    #[test]
    fn every_frame_carries_the_buffer_epoch() {
        let mut ring = RingBuffer::new(42, 10);
        let frame = ring.push(body());
        assert_eq!(frame.epoch, 42);
    }

    #[test]
    fn overflow_drops_the_oldest_entry() {
        let mut ring = RingBuffer::new(1, 3);
        for _ in 0..5 {
            ring.push(body());
        }
        let replayed = ring.replay_since(None).unwrap();
        let seqs: Vec<u64> = replayed.iter().map(|f| f.seq).collect();
        assert_eq!(
            seqs,
            vec![2, 3, 4],
            "only the 3 most recent entries survive"
        );
    }

    #[test]
    fn replay_none_cursor_returns_everything_retained() {
        let mut ring = RingBuffer::new(1, 10);
        for _ in 0..3 {
            ring.push(body());
        }
        assert_eq!(ring.replay_since(None).unwrap().len(), 3);
    }

    #[test]
    fn replay_with_cursor_returns_only_newer_frames() {
        let mut ring = RingBuffer::new(1, 10);
        for _ in 0..5 {
            ring.push(body());
        }
        let replayed = ring.replay_since(Some(2)).unwrap();
        let seqs: Vec<u64> = replayed.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, vec![3, 4]);
    }

    #[test]
    fn cursor_past_window_reports_window_exceeded() {
        let mut ring = RingBuffer::new(1, 2);
        for _ in 0..5 {
            ring.push(body()); // retains only seq 3,4 — seq 0,1,2 dropped
        }
        assert!(
            ring.replay_since(Some(0)).is_err(),
            "a cursor before the retained window must be rejected"
        );
    }

    #[test]
    fn cursor_at_exact_boundary_of_retained_window_is_not_exceeded() {
        let mut ring = RingBuffer::new(1, 2);
        for _ in 0..5 {
            ring.push(body()); // retains seq 3,4
        }
        // Client's cursor is 2 (the seq just before the oldest retained, 3) — contiguous,
        // nothing lost.
        assert_eq!(
            ring.replay_since(Some(2))
                .unwrap()
                .iter()
                .map(|f| f.seq)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    #[test]
    fn fresh_empty_buffer_with_a_cursor_ahead_of_next_seq_is_not_falsely_flagged() {
        // Nothing has ever been pushed — next_seq is 0, so a (bogus) cursor of e.g. 0 must not
        // be treated as a gap simply because the buffer is empty.
        let ring = RingBuffer::new(1, 10);
        assert!(ring
            .replay_since(Some(0))
            .expect("must not be window-exceeded")
            .is_empty());
    }
}
