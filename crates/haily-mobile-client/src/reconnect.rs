//! Pure, network-free reconnect logic (red team C4/M8/M9): the resume cursor sent in `Hello`,
//! per-connection seq dedup (including the epoch-mismatch reset), and exponential backoff for
//! the reconnect-on-foreground loop. Kept separate from `ws.rs`/`client.rs` so these invariants
//! are unit-testable without spinning up any socket at all.
use haily_types::{ClientFrame, PROTOCOL_VERSION};
use std::time::Duration;

/// What the client remembers across a disconnect so `Hello` can ask for exactly the right
/// resume (or a fresh full resync). `None`/`None` on first-ever connect (§5 of the protocol doc).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResumeCursor {
    pub last_epoch: Option<u64>,
    pub last_seen_seq: Option<u64>,
}

impl ResumeCursor {
    pub fn fresh() -> Self {
        Self::default()
    }

    /// Builds the `Hello` frame this cursor implies. Always sent as the first frame after a
    /// (re)connect — the server decides replay/full-resync/`ResumeWindowExceeded` from these
    /// two fields plus its own current epoch (§6).
    pub fn hello_frame(&self) -> ClientFrame {
        ClientFrame::Hello {
            last_seen_seq: self.last_seen_seq,
            last_epoch: self.last_epoch,
            protocol_version: PROTOCOL_VERSION,
        }
    }

    /// Discards the seq cursor entirely while remembering the NEW epoch (§6.2) — called once a
    /// `HelloAck` reports an epoch different from what this cursor last saw. The old `seq` space
    /// is meaningless against the new one; comparing across the boundary would silently drop
    /// every live frame (C4), so resetting rather than merely "not updating" is the fix.
    pub fn reset_to_new_epoch(&mut self, epoch: u64) {
        self.last_epoch = Some(epoch);
        self.last_seen_seq = None;
    }

    /// Advances the cursor after successfully delivering (or deliberately skipping-as-duplicate)
    /// a frame at `seq` in `epoch` — every frame type consumes a seq slot (§2.2), so this is
    /// called for every accepted frame, not only `Chunk`/`Run`.
    pub fn advance(&mut self, epoch: u64, seq: u64) {
        self.last_epoch = Some(epoch);
        self.last_seen_seq = Some(seq);
    }
}

/// Per-connection seq dedup (M8/M9): the single-writer server assigns one strictly-increasing
/// `seq` per connection across every frame type, so the client only needs "is this newer than
/// what I've already accepted" — no per-type bookkeeping. An epoch change is NOT a duplicate
/// question; it is handled as "this is definitionally a different, fresh space" (§6.2).
#[derive(Debug, Default)]
pub struct SeqDedup {
    cursor: ResumeCursor,
}

impl SeqDedup {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cursor(&self) -> ResumeCursor {
        self.cursor
    }

    /// Returns `true` if this `(epoch, seq)` pair should be delivered to the app layer (and
    /// updates internal state to reflect having seen it); `false` for an already-seen/stale
    /// seq within the SAME epoch (a replay, e.g. from an overlapping resume window). A frame in
    /// a NEW epoch is always accepted — the old cursor told us nothing about this epoch's space.
    pub fn accept(&mut self, epoch: u64, seq: u64) -> bool {
        let is_new_epoch = self.cursor.last_epoch != Some(epoch);
        if is_new_epoch {
            self.cursor.reset_to_new_epoch(epoch);
            self.cursor.advance(epoch, seq);
            return true;
        }
        let is_newer = match self.cursor.last_seen_seq {
            None => true,
            Some(last) => seq > last,
        };
        if is_newer {
            self.cursor.advance(epoch, seq);
        }
        is_newer
    }
}

/// Exponential backoff with a cap, for the reconnect-on-foreground loop (researcher-01: sockets
/// die when backgrounded on both platforms, so reconnect only needs to be prompt on
/// foreground/first-attempt, not resilient to indefinite background retry storms).
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    attempt: u32,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            attempt: 0,
        }
    }

    /// Standard mobile-client defaults: start at 500ms, cap at 30s.
    pub fn with_defaults() -> Self {
        Self::new(Duration::from_millis(500), Duration::from_secs(30))
    }

    /// The delay to wait before the NEXT attempt, then advances internal state. `2^attempt *
    /// base`, capped at `max` — saturates rather than overflowing for a very long run of
    /// failures.
    pub fn next_delay(&mut self) -> Duration {
        let multiplier = 1u32.checked_shl(self.attempt).unwrap_or(u32::MAX);
        let scaled = self.base.saturating_mul(multiplier);
        self.attempt = self.attempt.saturating_add(1);
        scaled.min(self.max)
    }

    /// Called on a successful connect — the next disconnect starts backing off from `base`
    /// again rather than continuing to escalate from wherever a previous outage left off.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_cursor_sends_none_none_hello() {
        let cursor = ResumeCursor::fresh();
        assert!(matches!(
            cursor.hello_frame(),
            ClientFrame::Hello {
                last_seen_seq: None,
                last_epoch: None,
                ..
            }
        ));
    }

    #[test]
    fn advance_then_hello_carries_the_stored_cursor() {
        let mut cursor = ResumeCursor::fresh();
        cursor.advance(9, 500);
        assert!(matches!(
            cursor.hello_frame(),
            ClientFrame::Hello {
                last_seen_seq: Some(500),
                last_epoch: Some(9),
                ..
            }
        ));
    }

    #[test]
    fn epoch_reset_discards_seq_but_remembers_new_epoch() {
        let mut cursor = ResumeCursor::fresh();
        cursor.advance(7, 500);
        cursor.reset_to_new_epoch(8);
        assert_eq!(cursor.last_epoch, Some(8));
        assert_eq!(cursor.last_seen_seq, None);
    }

    #[test]
    fn dedup_accepts_strictly_increasing_seq_within_one_epoch() {
        let mut dedup = SeqDedup::new();
        assert!(dedup.accept(1, 1));
        assert!(dedup.accept(1, 2));
        assert!(dedup.accept(1, 3));
    }

    #[test]
    fn dedup_rejects_a_repeated_or_older_seq_within_one_epoch() {
        let mut dedup = SeqDedup::new();
        assert!(dedup.accept(1, 5));
        assert!(!dedup.accept(1, 5), "exact repeat must be rejected");
        assert!(!dedup.accept(1, 3), "an older seq must be rejected");
        assert!(dedup.accept(1, 6), "a genuinely newer seq is accepted");
    }

    #[test]
    fn dedup_always_accepts_a_new_epoch_even_with_a_low_seq() {
        let mut dedup = SeqDedup::new();
        assert!(dedup.accept(7, 500));
        // Server restarted: epoch 7 -> 8, seq resets to 1 — this is NOT a stale duplicate (C4).
        assert!(dedup.accept(8, 1));
        assert_eq!(dedup.cursor().last_epoch, Some(8));
        assert_eq!(dedup.cursor().last_seen_seq, Some(1));
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(), Duration::from_millis(100));
        assert_eq!(backoff.next_delay(), Duration::from_millis(200));
        assert_eq!(backoff.next_delay(), Duration::from_millis(400));
        assert_eq!(backoff.next_delay(), Duration::from_millis(800));
        // Would be 1600ms uncapped — must clamp to the 1s max.
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn backoff_reset_restarts_from_base() {
        let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_secs(1));
        backoff.next_delay();
        backoff.next_delay();
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(100));
    }
}
