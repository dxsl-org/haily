//! Registry of in-flight turn cancellation tokens, keyed by session id.
//!
//! `dispatch.rs` mints a per-turn `CancellationToken` (a child of the root shutdown
//! token) for every request and registers it here so a UI-facing "Stop" action can
//! cancel a specific turn by `session_id` without reaching into the dispatch loop or
//! the orchestrator. This is purely a registration/lookup layer — the actual
//! cancellation plumbing (llama/cloud stream honoring the token) is Phase 6's.
use dashmap::DashMap;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Maps `session_id` → that turn's `CancellationToken`. Cheaply cloneable (all state
/// is inside the `DashMap`, itself unwrapped rather than `Arc`-wrapped here — callers
/// share one instance via `Arc<TurnRegistry>`, mirroring `AdapterManager`'s pattern of
/// keeping the concurrent map as the sole piece of shared state).
#[derive(Debug, Default)]
pub struct TurnRegistry {
    tokens: DashMap<Uuid, CancellationToken>,
}

impl TurnRegistry {
    pub fn new() -> Self {
        Self {
            tokens: DashMap::new(),
        }
    }

    /// Register `token` as the cancellable handle for `session_id`'s in-flight turn.
    /// Overwrites any prior entry for the same id — sessions are one-turn-at-a-time
    /// (the composer blocks a second send while one is pending), so an overwrite
    /// would only happen if a caller reused a session id across turns.
    pub fn register(&self, session_id: Uuid, token: CancellationToken) {
        self.tokens.insert(session_id, token);
    }

    /// Remove `session_id`'s entry without cancelling it — used on normal turn
    /// completion so the map doesn't grow unbounded with tokens for turns that
    /// already finished.
    pub fn remove(&self, session_id: Uuid) {
        self.tokens.remove(&session_id);
    }

    /// Cancel `session_id`'s in-flight turn, if any. Removes the entry either way (a
    /// cancelled turn is about to exit and clean up on its own exit path, but
    /// removing here too makes a duplicate `cancel_turn` call a safe no-op instead of
    /// re-firing an already-cancelled token). Returns `true` if a turn was found and
    /// cancelled, `false` if `session_id` had no registered turn (already finished,
    /// unknown, or never started).
    pub fn cancel(&self, session_id: Uuid) -> bool {
        match self.tokens.remove(&session_id) {
            Some((_, token)) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Number of turns currently registered — exposed for tests only (leak checks).
    /// Named `registered_count` rather than `len` since this is a test-only probe,
    /// not a collection-like API (no matching `is_empty`/iteration is intended).
    #[cfg(test)]
    pub fn registered_count(&self) -> usize {
        self.tokens.len()
    }
}

/// Mobile Thin-Client plan phase 3 amendment (see `docs/mobile-protocol.md` §3.2 and
/// phase-01/phase-03's cross-referenced Deviation Log entries): lets `haily-io`'s
/// `MobileAdapter` cancel a turn via `haily_types::TurnCanceller` without this crate's
/// `TurnRegistry` type leaking into the lower `haily-io` layer. Delegates straight to the
/// existing inherent `cancel` method — same semantics `src-tauri`'s desktop `cancel_turn`
/// command already exercises.
impl haily_types::TurnCanceller for TurnRegistry {
    fn cancel(&self, session_id: Uuid) -> bool {
        TurnRegistry::cancel(self, session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_fires_the_registered_token_and_removes_the_entry() {
        let registry = TurnRegistry::new();
        let session_id = Uuid::new_v4();
        let token = CancellationToken::new();
        registry.register(session_id, token.clone());

        assert!(
            registry.cancel(session_id),
            "should find and cancel the registered turn"
        );
        assert!(
            token.is_cancelled(),
            "the original token handle must observe the cancellation"
        );
        assert_eq!(
            registry.registered_count(),
            0,
            "cancel must remove the entry, not just fire it"
        );
    }

    #[test]
    fn cancel_on_unknown_session_returns_false() {
        let registry = TurnRegistry::new();
        assert!(
            !registry.cancel(Uuid::new_v4()),
            "no turn registered — must return false, not panic"
        );
    }

    #[test]
    fn remove_drops_the_entry_without_cancelling() {
        let registry = TurnRegistry::new();
        let session_id = Uuid::new_v4();
        let token = CancellationToken::new();
        registry.register(session_id, token.clone());

        registry.remove(session_id);

        assert!(
            !token.is_cancelled(),
            "remove is for normal completion — must not cancel"
        );
        assert_eq!(
            registry.registered_count(),
            0,
            "remove must drop the entry so the map doesn't leak"
        );
    }
}
