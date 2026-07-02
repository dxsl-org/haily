//! Minimal per-key circuit breaker for `CloudClient`.
//!
//! Contract: 3 consecutive *transport-level* failures (connection/timeout — never an
//! HTTP status code) open the breaker for 30s. While open, the key is skipped during
//! rotation. After the open window elapses, exactly one probe is allowed through
//! (half-open); success closes the breaker and resets the failure count, failure
//! re-opens it for another 30s. HTTP 429 is a routing signal (try the next key) and
//! must never call `record_failure` — it is not a transport failure and tripping the
//! breaker on it would needlessly blacklist a key that is merely rate-limited.
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant};

const FAILURE_THRESHOLD: u8 = 3;
const OPEN_DURATION: Duration = Duration::from_secs(30);

/// One breaker per API key. `probe_in_flight` prevents multiple concurrent callers
/// from all treating the same half-open window as "my turn to probe" — only the
/// caller that flips `false -> true` proceeds; the rest see the key as still open.
struct KeyBreaker {
    consecutive_failures: AtomicU8,
    open_until: RwLock<Option<Instant>>,
    probe_in_flight: AtomicBool,
}

impl KeyBreaker {
    fn new() -> Self {
        Self {
            consecutive_failures: AtomicU8::new(0),
            open_until: RwLock::new(None),
            probe_in_flight: AtomicBool::new(false),
        }
    }
}

/// Per-key circuit breaker state for `CloudClient`. Sized at construction to match
/// `api_keys.len()`; index `i` in every method corresponds to `api_keys[i]`.
pub struct CircuitBreaker {
    keys: Vec<KeyBreaker>,
}

/// Outcome of `try_acquire` — whether the caller may attempt a request on this key
/// right now, distinguishing a fresh probe (must report the result) from routine
/// closed-state traffic (also reports, but there's no half-open slot to release).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Breaker is closed — proceed normally.
    Allowed,
    /// Breaker is open and the 30s window hasn't elapsed (or another caller already
    /// holds the probe slot) — skip this key.
    Blocked,
    /// Breaker was open, the window elapsed, and this caller won the probe slot —
    /// proceed, and the result of this one attempt determines close vs. re-open.
    Probing,
}

impl CircuitBreaker {
    pub fn new(key_count: usize) -> Self {
        Self { keys: (0..key_count).map(|_| KeyBreaker::new()).collect() }
    }

    /// Call before attempting a request on `idx`. Must be paired with exactly one of
    /// `record_success` / `record_failure` when the returned admission is not
    /// `Blocked` — otherwise a probe slot leaks and the key stays open forever.
    pub fn try_acquire(&self, idx: usize) -> Admission {
        let breaker = &self.keys[idx];
        let open_until = *breaker.open_until.read().unwrap_or_else(|e| e.into_inner());
        match open_until {
            None => Admission::Allowed,
            Some(until) if Instant::now() < until => Admission::Blocked,
            Some(_) => {
                // Window elapsed — at most one caller gets to probe. compare_exchange
                // ensures concurrent rotators don't all pile onto the same half-open key.
                if breaker
                    .probe_in_flight
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    Admission::Probing
                } else {
                    Admission::Blocked
                }
            }
        }
    }

    /// Transport-level success (or any non-transport outcome, incl. HTTP 429/4xx/5xx
    /// that reached the server) — closes the breaker and resets the failure count.
    pub fn record_success(&self, idx: usize) {
        let breaker = &self.keys[idx];
        breaker.consecutive_failures.store(0, Ordering::Relaxed);
        *breaker.open_until.write().unwrap_or_else(|e| e.into_inner()) = None;
        breaker.probe_in_flight.store(false, Ordering::Release);
    }

    /// Transport-level failure ONLY (connect/timeout/DNS — never an HTTP status).
    /// Increments the streak; opens for `OPEN_DURATION` once the streak hits
    /// `FAILURE_THRESHOLD`. A failed probe re-opens immediately (streak is already
    /// at/above threshold) rather than requiring three more failures.
    pub fn record_failure(&self, idx: usize) {
        let breaker = &self.keys[idx];
        let prev = breaker.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        // Set the open deadline BEFORE releasing the probe slot: releasing first opens
        // a ns-scale window where a concurrent `try_acquire` could grab the probe slot
        // and be admitted even though this failure should have re-opened the circuit.
        if prev + 1 >= FAILURE_THRESHOLD {
            *breaker.open_until.write().unwrap_or_else(|e| e.into_inner()) =
                Some(Instant::now() + OPEN_DURATION);
        }
        breaker.probe_in_flight.store(false, Ordering::Release);
    }

    /// An attempt reached the server but produced a result that is neither a clean
    /// success nor a transport failure — currently only HTTP 429, which is a routing
    /// signal and must not move the failure streak either way. Releases the probe
    /// slot (a no-op if this key wasn't in `Probing`) so a 429 during a half-open
    /// probe doesn't permanently wedge the breaker open with no future probe ever
    /// admitted.
    pub fn record_inconclusive(&self, idx: usize) {
        self.keys[idx].probe_in_flight.store(false, Ordering::Release);
    }

    #[cfg(test)]
    fn force_open_until(&self, idx: usize, until: Instant) {
        let breaker = &self.keys[idx];
        breaker.consecutive_failures.store(FAILURE_THRESHOLD, Ordering::Relaxed);
        *breaker.open_until.write().unwrap_or_else(|e| e.into_inner()) = Some(until);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_by_default() {
        let cb = CircuitBreaker::new(2);
        assert_eq!(cb.try_acquire(0), Admission::Allowed);
        assert_eq!(cb.try_acquire(1), Admission::Allowed);
    }

    #[test]
    fn two_failures_do_not_open() {
        let cb = CircuitBreaker::new(1);
        cb.record_failure(0);
        cb.record_failure(0);
        assert_eq!(cb.try_acquire(0), Admission::Allowed);
    }

    #[test]
    fn three_consecutive_failures_open_the_breaker() {
        let cb = CircuitBreaker::new(1);
        cb.record_failure(0);
        cb.record_failure(0);
        cb.record_failure(0);
        assert_eq!(cb.try_acquire(0), Admission::Blocked);
    }

    #[test]
    fn success_resets_failure_streak() {
        let cb = CircuitBreaker::new(1);
        cb.record_failure(0);
        cb.record_failure(0);
        cb.record_success(0);
        cb.record_failure(0);
        cb.record_failure(0);
        // Only 2 consecutive failures since the reset — still closed.
        assert_eq!(cb.try_acquire(0), Admission::Allowed);
    }

    #[test]
    fn open_key_does_not_block_other_keys() {
        let cb = CircuitBreaker::new(2);
        cb.record_failure(0);
        cb.record_failure(0);
        cb.record_failure(0);
        assert_eq!(cb.try_acquire(0), Admission::Blocked);
        assert_eq!(cb.try_acquire(1), Admission::Allowed);
    }

    #[test]
    fn probe_allowed_after_window_elapses() {
        let cb = CircuitBreaker::new(1);
        // Force-open with a window that has already elapsed (avoids a real 30s sleep).
        cb.force_open_until(0, Instant::now() - Duration::from_millis(1));
        assert_eq!(cb.try_acquire(0), Admission::Probing);
    }

    #[test]
    fn only_one_probe_admitted_per_open_window() {
        let cb = CircuitBreaker::new(1);
        cb.force_open_until(0, Instant::now() - Duration::from_millis(1));
        assert_eq!(cb.try_acquire(0), Admission::Probing);
        // A second concurrent caller must not also get a probe slot.
        assert_eq!(cb.try_acquire(0), Admission::Blocked);
    }

    #[test]
    fn successful_probe_closes_the_breaker() {
        let cb = CircuitBreaker::new(1);
        cb.force_open_until(0, Instant::now() - Duration::from_millis(1));
        assert_eq!(cb.try_acquire(0), Admission::Probing);
        cb.record_success(0);
        assert_eq!(cb.try_acquire(0), Admission::Allowed);
    }

    #[test]
    fn failed_probe_reopens_immediately() {
        let cb = CircuitBreaker::new(1);
        cb.force_open_until(0, Instant::now() - Duration::from_millis(1));
        assert_eq!(cb.try_acquire(0), Admission::Probing);
        cb.record_failure(0);
        assert_eq!(cb.try_acquire(0), Admission::Blocked);
    }

    #[test]
    fn still_open_before_window_elapses() {
        let cb = CircuitBreaker::new(1);
        cb.force_open_until(0, Instant::now() + Duration::from_secs(30));
        assert_eq!(cb.try_acquire(0), Admission::Blocked);
    }

    #[test]
    fn inconclusive_probe_releases_slot_without_reopening_or_closing() {
        let cb = CircuitBreaker::new(1);
        cb.force_open_until(0, Instant::now() - Duration::from_millis(1));
        assert_eq!(cb.try_acquire(0), Admission::Probing);
        cb.record_inconclusive(0); // e.g. the probe hit a 429
        // Slot released, but open_until is untouched and already elapsed — the very
        // next acquire re-enters probing rather than being permanently blocked.
        assert_eq!(cb.try_acquire(0), Admission::Probing);
    }
}
