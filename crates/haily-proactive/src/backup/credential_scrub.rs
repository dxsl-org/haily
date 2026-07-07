//! Scrubs a plaintext connector-credential preference row out of a standalone backup
//! COPY (never the live database) when boot-time credential-migration status is not
//! clean — see `mod.rs`'s "Credential posture" note.
//!
//! Exists because withholding the scheduled backup indefinitely (the previous design,
//! triggered when the OS keyring is persistently unavailable and a connector
//! credential exists) sacrifices this whole worker's headline guarantee — durability —
//! for credential posture. Scrubbing the copy instead keeps BOTH invariants: life
//! memory is never left unbacked-up, and no plaintext credential ever ships in a
//! backup file (M7b).
use anyhow::{Context, Result};
use haily_db::queries::meta;
use std::path::Path;

/// Deletes each of `keys` from the backup copy at `copy_path`, then `VACUUM`s it so the
/// deleted rows' bytes are actually rewritten out of the file — a bare `DELETE` only
/// unlinks a row from the b-tree; the freed page can still hold the old bytes until the
/// next `VACUUM`. Mirrors `CredentialStore::scrub_residue`'s checkpoint+VACUUM contract
/// on the live DB, applied here to a standalone copy instead.
///
/// Deletes the row rather than blanking its value: on restore, an ABSENT credential
/// preference is indistinguishable from a fresh install that never configured the
/// connector — `bootstrap.rs` already treats a missing row (`Ok(None)`) as the clean
/// case, so this reuses that same semantics instead of inventing a new "blanked" state
/// the rest of the codebase would need to learn about.
///
/// # Errors
/// Returns an error if the copy cannot be reopened, a `DELETE` fails, or the `VACUUM`
/// fails. Callers MUST treat any error as "this copy may still contain plaintext" and
/// refuse to promote or keep it — see `run_cycle` in `mod.rs`.
pub async fn scrub_credential_preferences(copy_path: &Path, keys: &[String]) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let copy_db = haily_db::open_standalone_copy_for_maintenance(copy_path)
        .await
        .with_context(|| format!("reopening backup copy for credential scrub: {}", copy_path.display()))?;
    for key in keys {
        meta::delete_preference(&copy_db, key)
            .await
            .with_context(|| format!("deleting credential preference '{key}' from backup copy"))?;
    }
    copy_db.vacuum().await.context("VACUUM on backup copy after credential scrub")
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::DbHandle;

    /// The core invariant this module exists for: a plaintext credential seeded on the
    /// LIVE db must be gone from the backup COPY after scrubbing, while the live source
    /// (which this function must never touch) keeps it untouched.
    #[tokio::test]
    async fn scrub_removes_the_named_key_and_leaves_the_live_source_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live_db = DbHandle::init(&dir.path().join("haily.db")).await.expect("init live db");
        meta::upsert_preference(&live_db, "connector.odoo.api_key", "sk-plaintext-secret", "test")
            .await
            .expect("seed plaintext row");

        let copy_path = dir.path().join("daily.db");
        live_db.backup_to(&copy_path).await.expect("backup_to");

        scrub_credential_preferences(&copy_path, &["connector.odoo.api_key".to_string()])
            .await
            .expect("scrub must succeed");

        let reopened = DbHandle::init(&copy_path).await.expect("reopen scrubbed copy");
        let scrubbed_value =
            meta::get_preference(&reopened, "connector.odoo.api_key").await.expect("read scrubbed copy");
        assert!(scrubbed_value.is_none(), "credential preference must be gone from the backup copy");

        let live_value = meta::get_preference(&live_db, "connector.odoo.api_key").await.expect("read live db");
        assert_eq!(
            live_value.as_deref(),
            Some("sk-plaintext-secret"),
            "scrub must never touch the live database"
        );
    }

    #[tokio::test]
    async fn empty_key_list_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let copy_path = dir.path().join("daily.db");
        DbHandle::init(&copy_path).await.expect("init copy");
        scrub_credential_preferences(&copy_path, &[]).await.expect("no-op scrub must not error");
    }
}
