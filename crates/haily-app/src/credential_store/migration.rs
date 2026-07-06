//! One-time plaintext→keyring migration (M5c) — split out of `mod.rs` since it's a
//! distinct lifecycle concern from the steady-state get/set path, even though it's built
//! entirely out of calls to that same path (`set_secret`) plus a DB overwrite + scrub.
use super::marker::{is_keyring_marker, keyring_marker, SCRUB_CONFIRMED_PREF};
use super::CredentialStore;
use anyhow::{Context, Result};
use haily_db::queries::meta;

impl CredentialStore {
    /// One-time migration: move a plaintext secret currently held in `kms_preferences`
    /// under `cred_ref` into the keyring, overwrite the DB row with a reference marker, and
    /// re-run the WAL/freelist residue scrub until it is CONFIRMED (M6c).
    ///
    /// M6c fix: the marker write and the scrub confirmation are now separate steps with
    /// separate idempotency checks — a crash/SQLITE_BUSY between them used to strand
    /// plaintext residue forever, because the marker's OWN idempotency check (`raw` already
    /// holding the marker) short-circuited this whole function BEFORE the scrub could ever
    /// run a second time. Now: migrate the secret (skipped if already marker'd or nothing to
    /// migrate) THEN unconditionally attempt [`Self::ensure_scrubbed`], which is itself
    /// idempotent and safe to re-run every boot until its confirmation flag lands.
    ///
    /// # Errors
    /// Returns `Err` if the keyring write/read-your-write fails (the DB row is left
    /// UNTOUCHED in that case — no data loss on a failed migration) or if the residue
    /// scrub's checkpoint/VACUUM fails.
    pub async fn migrate_from_db(&self, cred_ref: &str) -> Result<()> {
        let raw = meta::get_preference(&self.db, cred_ref).await?;
        let already_migrated = raw.as_deref().is_some_and(is_keyring_marker);
        // Plaintext-if-any, filtered so a marker or empty row never reaches `set_secret`.
        let plaintext = raw.as_deref().filter(|v| !v.is_empty() && !is_keyring_marker(v));

        if let Some(secret) = plaintext {
            // Write to keyring FIRST; only overwrite the DB row once the secret is
            // verified readable back out. If this fails, the plaintext row is untouched —
            // no data loss.
            self.set_secret(cred_ref, secret)
                .await
                .with_context(|| format!("migration: failed to move '{cred_ref}' into keyring"))?;
            meta::upsert_preference(&self.db, cred_ref, &keyring_marker(cred_ref), "credential_migration")
                .await
                .with_context(|| format!("migration: failed to overwrite '{cred_ref}' with marker"))?;
            tracing::info!(cred_ref, "migrated credential from plaintext DB into OS keyring");
        } else if !already_migrated {
            return Ok(()); // Nothing to migrate, and no marker present to confirm-scrub either.
        }

        self.ensure_scrubbed()
            .await
            .with_context(|| format!("migration: residue scrub failed after migrating '{cred_ref}'"))
    }

    /// M6c: re-run the residue scrub until the SEPARATE `SCRUB_CONFIRMED_PREF` flag is set —
    /// idempotent and safe to call every boot regardless of which cred_ref triggered it (the
    /// scrub walks the WHOLE database file, not per-cred_ref residue). A scrub that itself
    /// fails partway must leave the flag unset so the NEXT boot retries — never marked
    /// confirmed on anything but success.
    async fn ensure_scrubbed(&self) -> Result<()> {
        if meta::get_preference(&self.db, SCRUB_CONFIRMED_PREF).await?.as_deref() == Some("true") {
            return Ok(());
        }
        self.scrub_residue().await?;
        meta::upsert_preference(&self.db, SCRUB_CONFIRMED_PREF, "true", "credential_migration").await?;
        Ok(())
    }

    /// After overwriting a plaintext secret's row, the OLD value still physically exists in
    /// the SQLite WAL and freelist pages until scrubbed. `wal_checkpoint(TRUNCATE)` folds
    /// committed WAL frames back into the main file and truncates the `-wal` file; `VACUUM`
    /// rebuilds the main file, dropping freed pages that could still hold the old bytes.
    /// Both must run for the scrub to be complete — checkpoint alone leaves the
    /// pre-overwrite page in the main file's free list.
    ///
    /// M7a (Phase 6, "Activate & Measure"): goes through `DbHandle::vacuum()` — now
    /// exposed there — rather than a raw `sqlx::query("VACUUM")` against the pool
    /// directly, so this scrub serializes behind the same whole-DB maintenance lock as
    /// the scheduled backup's `VACUUM INTO` and the daily-rollup `VACUUM`. Running two
    /// whole-DB rewrites concurrently is what M7a's maintenance lock exists to prevent.
    async fn scrub_residue(&self) -> Result<()> {
        self.db.wal_checkpoint_truncate().await?;
        self.db.vacuum().await?;
        Ok(())
    }
}
