//! Serialized, `spawn_blocking`-wrapped keyring RPC plumbing — split out of `mod.rs` so the
//! policy/fallback orchestration there isn't tangled up with `Entry` lifecycle mechanics.
//!
//! One `Entry` is built (and reused) per `cred_ref`, not freshly per call: a real OS
//! backend reconnects to the same persistent store by `(service, user)` name either way,
//! but the crate's own `mock` backend keeps a credential's password ONLY inside that
//! specific `Entry` instance with NO cross-instance persistence — a fresh `Entry::new` per
//! call would silently never round-trip under the mock. Caching the entry object is
//! therefore both correct against real backends and required for the mock to behave as
//! documented (see `keyring::mock`'s own doc comment).
use keyring::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// The keyring "service" namespace every Haily credential is stored under.
const SERVICE: &str = "haily";

/// Owns the serialization lock + per-`cred_ref` `Entry` cache. `rpc_lock` doubles as BOTH
/// the RPC serialization mutex (the platform backends are documented as unreliable under
/// concurrent access) and the guard for "look up or insert an Entry for this cred_ref"
/// being atomic — one lock, not two, since both operations need to happen together on
/// every call anyway.
#[derive(Default)]
pub(super) struct KeyringRpc {
    rpc_lock: Mutex<HashMap<String, Arc<Entry>>>,
}

impl KeyringRpc {
    /// Fetch (or lazily build) the cached `Entry` for `cred_ref`, under the lock that
    /// serializes RPC calls.
    async fn entry_for(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, HashMap<String, Arc<Entry>>>,
        cred_ref: &str,
    ) -> Result<Arc<Entry>, keyring::Error> {
        if let Some(entry) = guard.get(cred_ref) {
            return Ok(Arc::clone(entry));
        }
        let entry = Arc::new(Entry::new(SERVICE, cred_ref)?);
        guard.insert(cred_ref.to_string(), Arc::clone(&entry));
        Ok(entry)
    }

    /// One serialized, `spawn_blocking`-wrapped keyring read.
    pub async fn get(&self, cred_ref: &str) -> Result<String, keyring::Error> {
        let mut guard = self.rpc_lock.lock().await;
        let entry = self.entry_for(&mut guard, cred_ref).await?;
        tokio::task::spawn_blocking(move || entry.get_password())
            .await
            .unwrap_or_else(join_error_as_platform_failure)
    }

    /// One serialized, `spawn_blocking`-wrapped keyring write.
    pub async fn set(&self, cred_ref: &str, secret: &str) -> Result<(), keyring::Error> {
        let mut guard = self.rpc_lock.lock().await;
        let entry = self.entry_for(&mut guard, cred_ref).await?;
        let secret = secret.to_string();
        tokio::task::spawn_blocking(move || entry.set_password(&secret))
            .await
            .unwrap_or_else(join_error_as_platform_failure)
    }

    /// TEST-SUPPORT ONLY (`#[cfg(any(test, feature = "test-support"))]`, enabled for this
    /// crate's own unit tests and for `tests/credential_store.rs`'s external integration
    /// tests): force the NEXT keyring call on `cred_ref` to fail with `err`, acting on
    /// THIS store's own cached `Entry` (creating one first if `cred_ref` was never
    /// touched). Required because the mock backend's `MockCredential` has no
    /// cross-instance persistence (see the module doc) — a caller-side `Entry::new` on
    /// the same `(service, cred_ref)` would build an unrelated mock instance and silently
    /// have no effect on what this store actually calls. Never compiled into a release
    /// build (the feature is not on by default and no production Cargo.toml enables it).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn force_next_error(&self, cred_ref: &str, err: keyring::Error) {
        let mut guard = self.rpc_lock.lock().await;
        let entry = self
            .entry_for(&mut guard, cred_ref)
            .await
            .expect("entry_for never fails for the mock backend");
        let mock: &keyring::mock::MockCredential = entry.get_credential().downcast_ref().unwrap();
        mock.set_error(err);
    }
}

/// A `spawn_blocking` join failure (the blocking task panicked or was cancelled) has no
/// natural `keyring::Error` variant — surface it as `PlatformFailure` so callers don't need
/// a THIRD error shape beyond "keyring said no" and "the OS call itself broke".
fn join_error_as_platform_failure<T>(e: tokio::task::JoinError) -> Result<T, keyring::Error> {
    Err(keyring::Error::PlatformFailure(Box::new(
        std::io::Error::other(format!("spawn_blocking join failed: {e}")),
    )))
}
