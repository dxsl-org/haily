//! Paired mobile-device registry (Mobile Thin-Client plan phase 2a).
//!
//! `token_hash` is the SHA-256 hex of the device's bearer token — the plaintext token
//! is generated + returned exactly once, at pairing time, and never persisted (see
//! `haily-io::mobile::pairing`). Revocation is soft (`revoked_at`), matching the
//! existing `deleted_at` convention, so a device's pairing history stays auditable.
use crate::DbHandle;
use anyhow::Result;
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use uuid::Uuid;

/// A stored paired-device row.
#[derive(Debug, Clone, FromRow)]
pub struct DeviceRow {
    pub device_id: String,
    pub device_name: String,
    pub token_hash: String,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub revoked_at: Option<String>,
}

impl DeviceRow {
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
}

/// SHA-256 hex of a raw bearer token — the only form ever written to `devices.token_hash`.
/// Deterministic across runs (unlike `DefaultHasher`), which is required for the upgrade
/// path's `find_active_by_token_hash` lookup to work at all.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Insert a newly-paired device row. Called ONLY after the desktop's out-of-band
/// confirm prompt has been accepted (red team M4) — this function itself does not
/// gate on that, the caller (`pairing.rs`) does. `last_seen_at` starts `NULL`; it is
/// set by [`touch_last_seen`] on the device's first successful WS connect, and an
/// unconfirmed row (paired but never connected) is later swept by
/// [`reap_unconfirmed`].
///
/// # Errors
/// Returns an error if the insert fails.
pub async fn insert(
    db: &DbHandle,
    device_id: Uuid,
    device_name: &str,
    token_hash: &str,
) -> Result<DeviceRow> {
    let created_at = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, DeviceRow>(
        "INSERT INTO devices (device_id, device_name, token_hash, created_at, last_seen_at, revoked_at)
         VALUES (?, ?, ?, ?, NULL, NULL)
         RETURNING *",
    )
    .bind(device_id.to_string())
    .bind(device_name)
    .bind(token_hash)
    .bind(&created_at)
    .fetch_one(db.pool())
    .await?)
}

/// Resolve a bearer token to its device row — the WS upgrade auth check. Returns
/// `None` for an unknown token OR a revoked one (the two cases the caller must treat
/// identically: reject the upgrade before any 101 response, per the mobile protocol
/// spec's no-auth-no-data invariant).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn find_active_by_token_hash(
    db: &DbHandle,
    token_hash: &str,
) -> Result<Option<DeviceRow>> {
    Ok(sqlx::query_as::<_, DeviceRow>(
        "SELECT * FROM devices WHERE token_hash = ? AND revoked_at IS NULL",
    )
    .bind(token_hash)
    .fetch_optional(db.pool())
    .await?)
}

/// Cheap point lookup for the per-frame revoked check (red team m3) — a live
/// connection re-checks this on every session-scoped frame, not only at upgrade, so
/// a revoke takes effect on an already-open socket. `true` for an unknown device_id
/// too (fail closed: a device row that vanished is at least as untrusted as one
/// marked revoked).
///
/// # Errors
/// Returns an error if the query fails.
pub async fn is_revoked(db: &DbHandle, device_id: Uuid) -> Result<bool> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT revoked_at FROM devices WHERE device_id = ?")
            .bind(device_id.to_string())
            .fetch_optional(db.pool())
            .await?;
    Ok(match row {
        Some((revoked_at,)) => revoked_at.is_some(),
        None => true,
    })
}

/// Stamp `last_seen_at` to now — called on every successful WS connect (not every
/// frame; that would be a write per message). Silently succeeds if `device_id` is
/// unknown (nothing to touch).
///
/// # Errors
/// Returns an error if the update fails.
pub async fn touch_last_seen(db: &DbHandle, device_id: Uuid) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE devices SET last_seen_at = ? WHERE device_id = ?")
        .bind(&now)
        .bind(device_id.to_string())
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Soft-revoke a device — the Devices panel's (P2b) revoke action. The live-socket
/// close and next-upgrade-rejection are enforced by the server reading
/// [`is_revoked`]/[`find_active_by_token_hash`], not by this write itself.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn revoke(db: &DbHandle, device_id: Uuid) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE devices SET revoked_at = ? WHERE device_id = ? AND revoked_at IS NULL")
        .bind(&now)
        .bind(device_id.to_string())
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Every non-revoked device, most-recently-paired first — the Devices panel's list.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_active(db: &DbHandle) -> Result<Vec<DeviceRow>> {
    Ok(sqlx::query_as::<_, DeviceRow>(
        "SELECT * FROM devices WHERE revoked_at IS NULL ORDER BY created_at DESC",
    )
    .fetch_all(db.pool())
    .await?)
}

/// Delete device rows that completed pairing but never connected (`last_seen_at IS
/// NULL`) and are older than `grace`, expressed as an RFC3339 cutoff timestamp (red
/// team m6) — an abandoned or failed pairing must not linger forever as a phantom
/// "paired" row. Returns the number of rows removed.
///
/// # Errors
/// Returns an error if the delete fails.
pub async fn reap_unconfirmed(db: &DbHandle, older_than: &str) -> Result<u64> {
    let result = sqlx::query(
        "DELETE FROM devices WHERE last_seen_at IS NULL AND revoked_at IS NULL AND created_at < ?",
    )
    .bind(older_than)
    .execute(db.pool())
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Returns the `TempDir` guard alongside the handle — it must outlive every query below it,
    // or the directory (and the SQLite file in it) is deleted the instant this function returns.
    // Windows tolerates the dangling handle long enough for tests to pass anyway; Linux does
    // not, and every query fails closed with SQLITE_CANTOPEN ("unable to open database file").
    async fn test_db() -> (DbHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = DbHandle::init(&dir.path().join("t.db"))
            .await
            .expect("db init");
        (db, dir)
    }

    #[test]
    fn hash_token_is_deterministic_and_hex() {
        let a = hash_token("secret-token");
        let b = hash_token("secret-token");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(a, hash_token("different-token"));
    }

    #[tokio::test]
    async fn insert_then_find_active_by_token_hash_roundtrips() {
        let (db, _dir) = test_db().await;
        let device_id = Uuid::new_v4();
        let hash = hash_token("tok-1");
        insert(&db, device_id, "My Phone", &hash)
            .await
            .expect("insert");

        let found = find_active_by_token_hash(&db, &hash)
            .await
            .expect("query")
            .expect("device must be found");
        assert_eq!(found.device_id, device_id.to_string());
        assert_eq!(found.device_name, "My Phone");
        assert!(!found.is_revoked());
    }

    #[tokio::test]
    async fn unknown_token_hash_resolves_to_none() {
        let (db, _dir) = test_db().await;
        assert!(find_active_by_token_hash(&db, "nope")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn revoked_device_is_excluded_from_find_active() {
        let (db, _dir) = test_db().await;
        let device_id = Uuid::new_v4();
        let hash = hash_token("tok-2");
        insert(&db, device_id, "Phone", &hash).await.unwrap();
        revoke(&db, device_id).await.unwrap();

        assert!(find_active_by_token_hash(&db, &hash)
            .await
            .unwrap()
            .is_none());
        assert!(is_revoked(&db, device_id).await.unwrap());
    }

    #[tokio::test]
    async fn is_revoked_fails_closed_for_unknown_device() {
        let (db, _dir) = test_db().await;
        assert!(is_revoked(&db, Uuid::new_v4()).await.unwrap());
    }

    #[tokio::test]
    async fn touch_last_seen_updates_the_timestamp() {
        let (db, _dir) = test_db().await;
        let device_id = Uuid::new_v4();
        insert(&db, device_id, "Phone", &hash_token("tok-3"))
            .await
            .unwrap();

        touch_last_seen(&db, device_id).await.unwrap();
        let row = find_active_by_token_hash(&db, &hash_token("tok-3"))
            .await
            .unwrap()
            .unwrap();
        assert!(row.last_seen_at.is_some());
    }

    #[tokio::test]
    async fn list_active_excludes_revoked_devices() {
        let (db, _dir) = test_db().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        insert(&db, a, "A", &hash_token("a")).await.unwrap();
        insert(&db, b, "B", &hash_token("b")).await.unwrap();
        revoke(&db, b).await.unwrap();

        let active = list_active(&db).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].device_id, a.to_string());
    }

    #[tokio::test]
    async fn reap_unconfirmed_removes_only_never_connected_rows_past_cutoff() {
        let (db, _dir) = test_db().await;
        let stale = Uuid::new_v4();
        insert(&db, stale, "Stale", &hash_token("stale"))
            .await
            .unwrap();

        let connected = Uuid::new_v4();
        insert(&db, connected, "Connected", &hash_token("connected"))
            .await
            .unwrap();
        touch_last_seen(&db, connected).await.unwrap();

        // Cutoff far in the future — both "never connected" rows are eligible, but only
        // `stale` has no last_seen_at, so only it is removed.
        let cutoff = chrono::Utc::now()
            .checked_add_signed(chrono::Duration::hours(1))
            .unwrap()
            .to_rfc3339();
        let removed = reap_unconfirmed(&db, &cutoff).await.unwrap();
        assert_eq!(removed, 1);

        let active = list_active(&db).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].device_id, connected.to_string());
    }
}
