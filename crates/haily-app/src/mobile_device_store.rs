//! DB-backed [`haily_io::mobile::MobileDeviceStore`] (Mobile Thin-Client plan phase 2a) —
//! injected into `MobileAdapter` at construction, mirroring `session_transcript`'s
//! `DbSessionTranscript`: the leaf `haily-io` crate must not depend on `haily-db`, so the
//! persistence-backed implementation lives here at the app layer instead.
use async_trait::async_trait;
use haily_db::{queries::devices, DbHandle};
use haily_io::mobile::MobileDeviceStore;
use std::sync::Arc;
use uuid::Uuid;

pub struct DbMobileDeviceStore {
    db: Arc<DbHandle>,
}

impl DbMobileDeviceStore {
    pub fn new(db: Arc<DbHandle>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl MobileDeviceStore for DbMobileDeviceStore {
    async fn find_active_by_token_hash(&self, token_hash: &str) -> Option<Uuid> {
        match devices::find_active_by_token_hash(&self.db, token_hash).await {
            Ok(Some(row)) => Uuid::parse_str(&row.device_id).ok(),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("mobile: device token lookup failed: {e:#}");
                None
            }
        }
    }

    async fn is_revoked(&self, device_id: Uuid) -> bool {
        // Fail closed: a DB error is treated the same as "revoked" — an auth check that can't
        // prove a device is still valid must never treat that as "valid".
        devices::is_revoked(&self.db, device_id)
            .await
            .unwrap_or(true)
    }

    async fn touch_last_seen(&self, device_id: Uuid) {
        if let Err(e) = devices::touch_last_seen(&self.db, device_id).await {
            tracing::warn!("mobile: touch_last_seen failed for {device_id}: {e:#}");
        }
    }

    /// Review finding 3: `None` on a persistence failure — the caller (`pair_handler`) must
    /// never mint a token for a device row that was never actually written.
    async fn create_device(&self, device_name: &str, token_hash: &str) -> Option<Uuid> {
        let device_id = Uuid::new_v4();
        match devices::insert(&self.db, device_id, device_name, token_hash).await {
            Ok(_) => Some(device_id),
            Err(e) => {
                tracing::error!("mobile: failed to persist newly-paired device: {e:#}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> DbMobileDeviceStore {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DbHandle::init(&dir.path().join("t.db")).await.unwrap());
        DbMobileDeviceStore::new(db)
    }

    #[tokio::test]
    async fn create_then_find_by_token_hash_roundtrips() {
        let store = store().await;
        let hash = devices::hash_token("tok");
        let device_id = store
            .create_device("Phone", &hash)
            .await
            .expect("create must succeed");

        assert_eq!(
            store.find_active_by_token_hash(&hash).await,
            Some(device_id)
        );
        assert!(!store.is_revoked(device_id).await);
    }

    #[tokio::test]
    async fn unknown_device_id_is_revoked_fail_closed() {
        let store = store().await;
        assert!(store.is_revoked(Uuid::new_v4()).await);
    }

    #[tokio::test]
    async fn touch_last_seen_does_not_panic_on_an_unknown_device() {
        let store = store().await;
        store.touch_last_seen(Uuid::new_v4()).await; // no-op, must not panic
    }
}
