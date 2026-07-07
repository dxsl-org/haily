//! Connector manifest queries — the persistence layer for human-approved connectors.
//!
//! Manifests are HUMAN-ONLY to write (m3): there is no Tool/LLM path into `insert_version`
//! or `set_status`; the only callers are a human-invoked admin path (CLI/manual SQL) and
//! the read-only `list_active` the registry uses at startup. The append-only trigger
//! (migration 0013) makes `manifest_json`/`content_hash`/`version` immutable per version,
//! so an approved schema can never be silently mutated — a new schema is a new versioned
//! row requiring re-approval. `status` (active<->disabled) stays mutable for revocation.
//!
//! This table is NEVER referenced by any `kms_skills`/decay query (kms invariant) — it has
//! no confidence/EMA/archived_at, and nothing joins it to the skill path.
use crate::DbHandle;
use anyhow::Result;
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use uuid::Uuid;

/// A stored connector manifest row. `manifest_json` is the full serde document (the tool
/// layer owns its shape); `allowed_ip_cidrs` is a JSON array of pinned IP/CIDR strings
/// captured at approval time (C3 — never re-derived from the hostname at call time).
#[derive(Debug, Clone, FromRow)]
pub struct ConnectorManifestRow {
    pub id: String,
    pub connector_name: String,
    pub version: String,
    pub content_hash: String,
    pub manifest_json: String,
    pub base_url: String,
    pub allowed_ip_cidrs: String,
    pub status: String,
    pub created_at: String,
}

/// Deterministic content hash of a manifest document. SHA-256 hex of the exact bytes so
/// an out-of-band tamper of `manifest_json` is detectable on reload (hash mismatch =
/// re-approval). Deterministic across platforms/runs for identical input (unlike
/// `DefaultHasher`, whose SipHash seed is not stable enough for persistence).
pub fn content_hash(manifest_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(manifest_json.as_bytes());
    format!("{:x}", hasher.finalize())
}

impl ConnectorManifestRow {
    /// Recompute the hash of `manifest_json` and compare it to the stored `content_hash`.
    /// Returns `false` when they diverge — i.e. the manifest bytes were altered out-of-band
    /// (a raw sqlite write, a file-level DB edit, or a restore of a doctored DB — tamper
    /// that bypasses the append-only trigger, which only guards in-DB `UPDATE`s). The loader
    /// MUST skip a row that fails this so a tampered, human-unapproved schema never registers
    /// as a live connector; this is what makes "content-hashed integrity" a real check rather
    /// than a stored-but-unverified value.
    #[must_use]
    pub fn verify_integrity(&self) -> bool {
        content_hash(&self.manifest_json) == self.content_hash
    }
}

/// All `active` manifests, oldest first. Read-only; called by the orchestrator at startup
/// to register connector tools. A `disabled` manifest is intentionally excluded so a human
/// can revoke a connector without deleting its evidentiary row.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_active(db: &DbHandle) -> Result<Vec<ConnectorManifestRow>> {
    Ok(sqlx::query_as::<_, ConnectorManifestRow>(
        "SELECT * FROM connector_manifests WHERE status = 'active' ORDER BY created_at ASC",
    )
    .fetch_all(db.pool())
    .await?)
}

/// Every manifest row regardless of status — used by the GUI connector-config surface
/// (Phase 7) so a human can see (and re-enable) a `disabled` connector, unlike
/// [`list_active`], which the registry uses at startup and deliberately excludes them.
/// Read-only; still no Tool/LLM path reads or writes this.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_all(db: &DbHandle) -> Result<Vec<ConnectorManifestRow>> {
    Ok(sqlx::query_as::<_, ConnectorManifestRow>(
        "SELECT * FROM connector_manifests ORDER BY connector_name ASC, created_at DESC",
    )
    .fetch_all(db.pool())
    .await?)
}

/// Fetch one manifest by its (connector_name, version) approval unit. `None` if absent.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get_by_name_version(
    db: &DbHandle,
    connector_name: &str,
    version: &str,
) -> Result<Option<ConnectorManifestRow>> {
    Ok(sqlx::query_as::<_, ConnectorManifestRow>(
        "SELECT * FROM connector_manifests WHERE connector_name = ? AND version = ?",
    )
    .bind(connector_name)
    .bind(version)
    .fetch_optional(db.pool())
    .await?)
}

/// Fields for approving a new connector version. HUMAN-ONLY caller (m3).
pub struct NewManifest<'a> {
    pub connector_name: &'a str,
    pub version: &'a str,
    /// The full manifest document. `content_hash` is computed from this here.
    pub manifest_json: &'a str,
    pub base_url: &'a str,
    /// JSON array of pinned IP/CIDR strings (C3), captured at approval time.
    pub allowed_ip_cidrs: &'a str,
}

/// Insert a new connector version, computing `content_hash` from `manifest_json`. A row is
/// created `active`. Re-inserting an existing (connector_name, version) is a UNIQUE
/// conflict (immutable per version) — the caller must mint a new version to change a
/// schema, not re-insert.
///
/// # Errors
/// Returns an error on a UNIQUE(connector_name, version) conflict or any DB failure.
pub async fn insert_version(db: &DbHandle, m: NewManifest<'_>) -> Result<ConnectorManifestRow> {
    let id = Uuid::new_v4().to_string();
    let created_at = chrono::Utc::now().to_rfc3339();
    let hash = content_hash(m.manifest_json);
    Ok(sqlx::query_as::<_, ConnectorManifestRow>(
        "INSERT INTO connector_manifests
             (id, connector_name, version, content_hash, manifest_json, base_url,
              allowed_ip_cidrs, status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'active', ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(m.connector_name)
    .bind(m.version)
    .bind(&hash)
    .bind(m.manifest_json)
    .bind(m.base_url)
    .bind(m.allowed_ip_cidrs)
    .bind(&created_at)
    .fetch_one(db.pool())
    .await?)
}

/// Toggle a manifest's `status` (active<->disabled). Allowed by the trigger — revocation
/// must not require minting a new version. HUMAN-ONLY caller (m3).
///
/// # Errors
/// Returns an error if the update fails. Silently succeeds if no row matches `id`.
pub async fn set_status(db: &DbHandle, id: &str, status: &str) -> Result<()> {
    sqlx::query("UPDATE connector_manifests SET status = ? WHERE id = ?")
        .bind(status)
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}
