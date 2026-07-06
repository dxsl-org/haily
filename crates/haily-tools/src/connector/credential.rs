//! Credential-getter seam (Harness Completion phase 4) — lets an executor in this leaf
//! crate read a secret from whatever backing store the app layer configured (OS keyring,
//! with its own DB-fallback policy) WITHOUT `haily-tools` depending on `haily-app`.
//!
//! `haily-app` owns `credential_store::CredentialStore` (spawn_blocking keyring wrapper +
//! read/write fallback policy, M5). That crate sits ABOVE `haily-tools` in the dependency
//! graph (`haily-app` → `haily-core` → `haily-tools`), so the dependency can only point one
//! way: `haily-app` implements this trait and hands the executor a trait object at
//! construction time — the SAME injection pattern
//! [`crate::connector::http_connector_tool::HttpExecutor`] already uses for its own
//! `credential_getter` field, rather than the executor reaching out to a global.
use anyhow::Result;
use async_trait::async_trait;

/// Resolve a secret by its cred-by-reference NAME (e.g. `connector.odoo.api_key`). The
/// implementation decides where the secret actually lives (OS keyring, plaintext DB
/// fallback, env var, …) — the executor holding this trait object never needs to know.
///
/// # Errors
/// Returns `Err` only for an unexpected backing-store failure. A reference that simply
/// has no configured secret returns `Ok(None)`, not an error — the caller decides whether
/// "not configured" is fatal for its own operation.
#[async_trait]
pub trait CredentialGetter: Send + Sync {
    async fn get_secret(&self, cred_ref: &str) -> Result<Option<String>>;
}

#[cfg(test)]
pub mod mock {
    //! Test double for executor construction in unit tests that don't need a real
    //! `haily-app::CredentialStore` (or a DB at all).
    use super::*;
    use std::collections::HashMap;

    pub struct MockCredentialGetter(pub HashMap<String, String>);

    #[async_trait]
    impl CredentialGetter for MockCredentialGetter {
        async fn get_secret(&self, cred_ref: &str) -> Result<Option<String>> {
            Ok(self.0.get(cred_ref).cloned())
        }
    }
}
