//! Write side of the connector config UI (Phase 7) — see the parent module doc. Every
//! function here is HUMAN-only: none is registered as a `Tool`, so the agent/LLM loop has no
//! path to call any of them.
use crate::CredentialStore;
use anyhow::{ensure, Context, Result};
use haily_db::queries::connectors;
use haily_db::queries::meta;
use haily_db::DbHandle;
use haily_tools::connector::manifest;

/// Write/rotate a connector's credential. Delegates entirely to
/// [`CredentialStore::set_credential`], which writes to the OS keyring and scrubs any
/// overwritten plaintext's WAL/freelist residue — never lands the secret in SQLite.
///
/// # Errors
/// Returns an error if `secret` is empty or the underlying keyring/scrub operation fails.
pub async fn set_connector_credential(store: &CredentialStore, cred_ref: &str, secret: &str) -> Result<()> {
    ensure!(!secret.trim().is_empty(), "credential value must not be empty");
    store
        .set_credential(cred_ref, secret)
        .await
        .with_context(|| format!("failed to set credential for '{cred_ref}'"))
}

/// Toggle a manifest row's `status`. Does NOT take effect live — see the parent module's doc
/// comment on revocation liveness.
///
/// # Errors
/// Returns an error if `status` is not `"active"`/`"disabled"`, or the update fails.
pub async fn set_connector_status(db: &DbHandle, id: &str, status: &str) -> Result<()> {
    ensure!(status == "active" || status == "disabled", "status must be 'active' or 'disabled'");
    connectors::set_status(db, id, status).await
}

/// Record that a human has explicitly reviewed and accepted `version` as the approved
/// baseline for `connector_name` — clears a `ReapprovalState` banner. Writes only a
/// `kms_preferences` pointer row; never touches `manifest_json`/`content_hash`.
///
/// # Errors
/// Returns an error if the preference write fails.
pub async fn acknowledge_connector_version(db: &DbHandle, connector_name: &str, version: &str) -> Result<()> {
    let pref_key = manifest::approved_version_pref_key(connector_name);
    meta::upsert_preference(db, &pref_key, version, "connector_config_ack").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_connector_status_rejects_invalid_value() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        let err = set_connector_status(&db, "some-id", "revoked").await.unwrap_err();
        assert!(err.to_string().contains("active"));
    }

    #[tokio::test]
    async fn set_connector_credential_rejects_empty_secret() {
        keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());
        let store = CredentialStore::new(db, crate::CredentialPolicy::default());
        let err = set_connector_credential(&store, "connector.test.key", "   ").await.unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
