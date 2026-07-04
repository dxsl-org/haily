//! OS-keyring credential storage (Phase 4 of Harness Completion).
//!
//! Closes the Phase-13 "Decision 2 Risk Note": connector secrets move from plaintext
//! `kms_preferences` into the OS keyring (`keyring = "3"`), keeping the existing
//! cred-by-reference contract intact — callers still pass a reference NAME (e.g.
//! `connector.odoo.api_key`), never the secret itself.
//!
//! ## Fallback policy is asymmetric (M5b) — this is the security-critical property
//! - READ-fallback (reading a plaintext secret from `kms_preferences` when the keyring
//!   errors) is ALLOWED by default. Safe direction for a single-user local-first app: the
//!   threat model is casual disk/log scraping, not a co-resident attacker with DB access.
//! - WRITE-fallback (writing a NEW secret to plaintext DB when the keyring errors) is
//!   FAIL-CLOSED by default. The caller gets an `Err`, never a silent plaintext write,
//!   unless `allow_write_plaintext` is explicitly set in [`CredentialPolicy`].
//!
//! ## Headless/Session-0 (M5a)
//! Windows Credential Manager (DPAPI, tied to the interactive session) and Linux
//! secret-service (needs a D-Bus session bus) are both structurally unavailable in a true
//! daemon/Session-0 context. `--headless` sets [`CredentialPolicy::attempt_keyring`] to
//! `false` BEFORE any keyring call — the DB-read path is used directly, and a PERSISTED
//! warning flag is set (`credential.fallback_active` in `kms_preferences`) so the GUI can
//! surface it as a banner on next open. This is a policy decision, made once at startup;
//! it never bricks the daemon over an environment the daemon cannot fix.
//!
//! ## Concurrency
//! The keyring API is fully synchronous — every call goes through `spawn_blocking`, wrapped
//! by [`rpc::KeyringRpc`] which also serializes ALL keyring RPC calls through one
//! `tokio::sync::Mutex` (contention on the platform RPC mechanism can itself fail — see the
//! phase-4 research report). The first successful read is cached in-memory here (in
//! [`CredentialStore::cache`]) to avoid a per-request RPC round-trip; the cache is NEVER
//! logged and must never be added to a `Debug`/`Display` impl (hence [`CredentialStore`]
//! does not derive either).
mod marker;
mod migration;
mod policy;
mod rpc;
#[cfg(test)]
mod tests;

pub use marker::{is_keyring_marker, FALLBACK_WARNING_PREF, KEYRING_MARKER_PREFIX};
pub use policy::CredentialPolicy;
use rpc::KeyringRpc;

use anyhow::Result;
use haily_db::queries::meta;
use haily_db::DbHandle;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, warn};

/// Async wrapper over the OS keyring, with a serialized-RPC + first-read cache + the
/// asymmetric read/write fallback split (M5b).
///
/// Cheap to clone (`Arc` internals) — one instance is built at bootstrap and shared.
#[derive(Clone)]
pub struct CredentialStore {
    db: Arc<DbHandle>,
    policy: CredentialPolicy,
    rpc: Arc<KeyringRpc>,
    /// First-successful-read cache, keyed by cred_ref. In-memory ONLY: never persisted,
    /// never logged. Deliberately NOT part of any Debug/Display impl (this struct derives
    /// neither) so a stray `{:?}` on the store can never leak a cached secret.
    cache: Arc<Mutex<HashMap<String, String>>>,
}

impl CredentialStore {
    #[must_use]
    pub fn new(db: Arc<DbHandle>, policy: CredentialPolicy) -> Self {
        Self {
            db,
            policy,
            rpc: Arc::new(KeyringRpc::default()),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Read the secret referenced by `cred_ref` (e.g. `connector.odoo.api_key`).
    ///
    /// Order: in-memory cache → keyring (unless `attempt_keyring` is off) → DB read-fallback
    /// (allowed by default, M5b). A keyring `PlatformFailure`/other unexpected error is
    /// logged loudly AND persisted as a warning flag before the DB fallback is attempted.
    ///
    /// # Errors
    /// Returns `Err` only when the secret exists nowhere (no cache hit, no keyring entry,
    /// no DB row) or fallback is policy-disabled and the keyring is unusable.
    pub async fn get_secret(&self, cred_ref: &str) -> Result<Option<String>> {
        if let Some(cached) = self.cache.lock().await.get(cred_ref).cloned() {
            return Ok(Some(cached));
        }

        if !self.policy.attempt_keyring {
            // M5a: headless/Session-0 — go straight to the DB path, no keyring RPC.
            return self.read_db_plaintext(cred_ref).await;
        }

        match self.rpc.get(cred_ref).await {
            Ok(secret) => {
                self.cache
                    .lock()
                    .await
                    .insert(cred_ref.to_string(), secret.clone());
                Ok(Some(secret))
            }
            Err(keyring::Error::NoEntry) => {
                // Never migrated (or deleted) — not an error condition, just "ask the DB".
                self.read_db_plaintext(cred_ref).await
            }
            Err(e) => {
                warn!(cred_ref, error = %e, "keyring read failed");
                self.persist_fallback_warning().await;
                if self.policy.allow_read_fallback {
                    self.read_db_plaintext(cred_ref).await
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Write `secret` under `cred_ref` into the keyring.
    ///
    /// M5b: on a keyring write failure, this FAILS CLOSED (`Err`) unless
    /// `allow_write_plaintext` is explicitly set, in which case it writes plaintext to
    /// `kms_preferences` with a loud warning. A silent plaintext write on write-failure is
    /// the dangerous direction this function must never take by default.
    ///
    /// # Errors
    /// Returns `Err` if the keyring write fails and write-fallback is not opted in, or if
    /// the read-your-write verification after a successful write does not match.
    pub async fn set_secret(&self, cred_ref: &str, secret: &str) -> Result<()> {
        if !self.policy.attempt_keyring {
            // Headless still needs a way to persist a freshly-configured secret — since
            // the keyring is unreachable by policy, this degrades to the same fail-closed/
            // opt-in split as a keyring RPC failure, not an unconditional plaintext write.
            return self.write_fallback_or_fail_closed(cred_ref, secret).await;
        }

        match self.rpc.set(cred_ref, secret).await {
            Ok(()) => {
                // Read-your-write: some platform backends can silently no-op. Verify the
                // write actually landed before trusting it.
                match self.rpc.get(cred_ref).await {
                    Ok(readback) if readback == secret => {
                        self.cache
                            .lock()
                            .await
                            .insert(cred_ref.to_string(), secret.to_string());
                        Ok(())
                    }
                    Ok(_) => anyhow::bail!(
                        "keyring set_secret '{cred_ref}': read-your-write mismatch after write"
                    ),
                    Err(e) => {
                        anyhow::bail!("keyring set_secret '{cred_ref}': read-your-write failed: {e}")
                    }
                }
            }
            Err(e) => {
                warn!(cred_ref, error = %e, "keyring write failed");
                self.persist_fallback_warning().await;
                self.write_fallback_or_fail_closed(cred_ref, secret).await
            }
        }
    }

    /// M5b write-fallback split, shared by the headless-skip and keyring-error paths:
    /// opt-in plaintext write, else fail closed.
    async fn write_fallback_or_fail_closed(&self, cred_ref: &str, secret: &str) -> Result<()> {
        if !self.policy.allow_write_plaintext {
            anyhow::bail!(
                "cannot store credential '{cred_ref}': keyring unavailable and plaintext \
                 write-fallback is disabled (set the opt-in policy to allow it)"
            );
        }
        error!(
            cred_ref,
            "writing credential to PLAINTEXT DB fallback — keyring unavailable and write-fallback opt-in is set"
        );
        meta::upsert_preference(&self.db, cred_ref, secret, "credential_store_fallback").await?;
        self.cache
            .lock()
            .await
            .insert(cred_ref.to_string(), secret.to_string());
        Ok(())
    }

    /// Set the persisted fallback-warning flag (M5a/M5b) — read by the GUI on open.
    /// Best-effort: a failure to persist the flag must not fail the caller's actual
    /// read/write operation, so errors here are logged, not propagated.
    async fn persist_fallback_warning(&self) {
        if let Err(e) = meta::upsert_preference(&self.db, FALLBACK_WARNING_PREF, "true", "credential_store").await {
            warn!(error = %e, "failed to persist credential fallback warning flag");
        }
    }

    async fn read_db_plaintext(&self, cred_ref: &str) -> Result<Option<String>> {
        match meta::get_preference(&self.db, cred_ref).await? {
            Some(v) if !v.is_empty() && !is_keyring_marker(&v) => Ok(Some(v)),
            _ => Ok(None),
        }
    }

    /// TEST-SUPPORT ONLY (`#[cfg(any(test, feature = "test-support"))]`) — see
    /// [`KeyringRpc::force_next_error`] for why this must act on `self.rpc`'s own cached
    /// `Entry` rather than a caller-constructed one.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn force_next_keyring_error(&self, cred_ref: &str, err: keyring::Error) {
        self.rpc.force_next_error(cred_ref, err).await;
    }
}
