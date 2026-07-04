//! One-time plaintext→keyring migration (M5c) — split out of `mod.rs` since it's a
//! distinct lifecycle concern from the steady-state get/set path, even though it's built
//! entirely out of calls to that same path (`set_secret`) plus a DB overwrite + scrub.
use super::marker::{is_keyring_marker, keyring_marker};
use super::CredentialStore;
use anyhow::{Context, Result};
use haily_db::queries::meta;

impl CredentialStore {
    /// One-time migration: move a plaintext secret currently held in `kms_preferences`
    /// under `cred_ref` into the keyring, then overwrite the DB row with a reference
    /// marker and scrub the WAL/freelist residue (M5c).
    ///
    /// Idempotent: a row already holding the marker (checked via
    /// [`is_keyring_marker`]) is a no-op, so this is safe to call on every boot.
    ///
    /// # Errors
    /// Returns `Err` if the keyring write/read-your-write fails (the DB row is left
    /// UNTOUCHED in that case — no data loss on a failed migration) or if the residue
    /// scrub's checkpoint/VACUUM fails.
    pub async fn migrate_from_db(&self, cred_ref: &str) -> Result<()> {
        let Some(raw) = meta::get_preference(&self.db, cred_ref).await? else {
            return Ok(()); // Nothing to migrate.
        };
        if raw.is_empty() || is_keyring_marker(&raw) {
            return Ok(()); // Already migrated, or nothing real to move.
        }

        // Write to keyring FIRST; only overwrite the DB row once the secret is verified
        // readable back out. If this fails, the plaintext row is untouched — no data loss.
        self.set_secret(cred_ref, &raw)
            .await
            .with_context(|| format!("migration: failed to move '{cred_ref}' into keyring"))?;

        meta::upsert_preference(&self.db, cred_ref, &keyring_marker(cred_ref), "credential_migration")
            .await
            .with_context(|| format!("migration: failed to overwrite '{cred_ref}' with marker"))?;

        self.scrub_residue()
            .await
            .with_context(|| format!("migration: residue scrub failed after migrating '{cred_ref}'"))?;

        tracing::info!(cred_ref, "migrated credential from plaintext DB into OS keyring");
        Ok(())
    }

    /// M5c: after overwriting a plaintext secret's row, the OLD value still physically
    /// exists in the SQLite WAL and freelist pages until scrubbed. `wal_checkpoint(TRUNCATE)`
    /// folds committed WAL frames back into the main file and truncates the `-wal` file;
    /// `VACUUM` rebuilds the main file, dropping freed pages that could still hold the old
    /// bytes. Both must run for the scrub to be complete — checkpoint alone leaves the
    /// pre-overwrite page in the main file's free list.
    async fn scrub_residue(&self) -> Result<()> {
        self.db.wal_checkpoint_truncate().await?;
        // `VACUUM` is not exposed on `DbHandle` (out of this phase's file ownership) — run
        // it directly against the pool. It rebuilds the main DB file, dropping the freed
        // page(s) that previously held the overwritten plaintext secret; the checkpoint
        // above alone would leave that page sitting in the file's free list.
        sqlx::query("VACUUM").execute(self.db.pool()).await?;
        Ok(())
    }
}
