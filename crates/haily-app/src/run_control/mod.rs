//! Per-run kill/pause/resume control (Unified Chat UI phase 6, D3).
//!
//! [`RunControlRegistry`] mirrors `turns::TurnRegistry`'s shape (a `DashMap` keyed registry
//! shared via one `Arc`) but keyed by the pipeline `run_id` rather than a session id, and
//! holding TWO handles per entry instead of one: the run's `CancellationToken` (immediate kill —
//! stage sub-turns are children of it, so cancelling it cancels the in-flight stage, not just
//! the next boundary) and a `pause` flag (best-effort, checked only between stages).
//!
//! Registration happens SYNCHRONOUSLY in [`launch::spawn_launch`] — the ONE launch path both
//! `launch.rs` and `trigger.rs` call — so a `kill_run` issued between launch and the first
//! `RunStarted` event still has a token to cancel (see the phase's Key Insight). Cleanup happens
//! on ANY terminal-or-paused transition, driven from `watchers::spawn_run_event_bridge` (the
//! `RunEvent` stream already observes every such transition) — never here — so a token can never
//! leak for a paused/interrupted run, and a resumed run overwrites the SAME key with a fresh
//! token/flag pair (`register` overwrites, mirroring `TurnRegistry::register`'s own contract).
mod control;
mod launch;

pub use control::{is_resumable, kill_run, pause_run, resume_run};
pub use launch::{spawn_launch, LaunchCtx};

use dashmap::DashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct RunEntry {
    cancel: CancellationToken,
    pause: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct RunControlRegistry {
    entries: DashMap<String, RunEntry>,
}

impl RunControlRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or overwrite) `run_id`'s control handles. Overwriting by `run_id` is
    /// deliberate: a resumed run re-registers under the SAME id with a fresh token/flag pair, so
    /// a stale handle from the interrupted/paused attempt can never be mistaken for the live one.
    pub fn register(&self, run_id: &str, cancel: CancellationToken, pause: Arc<AtomicBool>) {
        self.entries
            .insert(run_id.to_string(), RunEntry { cancel, pause });
    }

    /// Drop `run_id`'s entry — called on ANY terminal-or-paused transition, never on a bare
    /// cancel/pause (which may legitimately be followed by a resume re-registering the same id).
    pub fn remove(&self, run_id: &str) {
        self.entries.remove(run_id);
    }

    /// Fire `run_id`'s cancellation token immediately, if registered. Returns whether an entry
    /// was found — `false` means the run is not (or no longer) in the registry, e.g. already
    /// terminal/paused (cleanup already ran) or never registered (an unknown/stale id).
    pub fn cancel(&self, run_id: &str) -> bool {
        match self.entries.get(run_id) {
            Some(entry) => {
                entry.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Flip `run_id`'s pause flag — the runner observes it at the next between-stages
    /// checkpoint (best-effort, never mid-stage). Returns whether an entry was found.
    pub fn set_pause(&self, run_id: &str) -> bool {
        match self.entries.get(run_id) {
            Some(entry) => {
                entry
                    .pause
                    .store(true, std::sync::atomic::Ordering::Release);
                true
            }
            None => false,
        }
    }

    /// Number of runs currently registered — test-only leak check (mirrors
    /// `TurnRegistry::registered_count`).
    #[cfg(test)]
    pub fn registered_count(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_fires_the_registered_token_without_removing_the_entry() {
        let registry = RunControlRegistry::new();
        let token = CancellationToken::new();
        registry.register("run1", token.clone(), Arc::new(AtomicBool::new(false)));

        assert!(
            registry.cancel("run1"),
            "should find and cancel the registered run"
        );
        assert!(
            token.is_cancelled(),
            "the original token handle must observe the cancellation"
        );
        assert_eq!(
            registry.registered_count(),
            1,
            "cancel alone must not remove the entry — cleanup is driven by the RunEvent bridge"
        );
    }

    #[test]
    fn cancel_on_unknown_run_returns_false() {
        let registry = RunControlRegistry::new();
        assert!(!registry.cancel("unknown"));
    }

    #[test]
    fn set_pause_flips_the_registered_flag() {
        let registry = RunControlRegistry::new();
        let pause = Arc::new(AtomicBool::new(false));
        registry.register("run1", CancellationToken::new(), Arc::clone(&pause));

        assert!(registry.set_pause("run1"));
        assert!(pause.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn register_overwrites_the_same_run_id() {
        let registry = RunControlRegistry::new();
        let first = CancellationToken::new();
        registry.register("run1", first.clone(), Arc::new(AtomicBool::new(false)));

        let second = CancellationToken::new();
        registry.register("run1", second.clone(), Arc::new(AtomicBool::new(false)));

        assert!(registry.cancel("run1"));
        assert!(
            second.is_cancelled(),
            "a resume's fresh token must be the one that actually fires"
        );
        assert!(
            !first.is_cancelled(),
            "the stale (pre-resume) token must never fire on its own"
        );
    }

    #[test]
    fn remove_drops_the_entry() {
        let registry = RunControlRegistry::new();
        registry.register(
            "run1",
            CancellationToken::new(),
            Arc::new(AtomicBool::new(false)),
        );
        registry.remove("run1");
        assert_eq!(registry.registered_count(), 0);
        assert!(!registry.cancel("run1"));
    }
}
