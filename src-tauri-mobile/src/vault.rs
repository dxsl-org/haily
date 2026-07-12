//! Device-token persistence via `iota_stronghold` (m8 decision — see the phase's Deviation
//! Log): the vault engine is used DIRECTLY from Rust command code — `tauri-plugin-stronghold`
//! itself is NOT registered, so there is no JS-invokable command surface for this vault at all;
//! the device token is never JS-reachable structurally, not merely by the frontend choosing not
//! to call one (Security Considerations: "Token in OS keystore only; never JS-reachable").
//!
//! **Honest threat-model note (review finding):** the vault's unlock key
//! ([`load_or_generate_vault_key`]) and its encrypted snapshot live in the SAME app-private
//! directory. Anyone who can read one of those two files (root/jailbreak, a backup tool that
//! copies the whole app data directory, physical extraction) can read the other, so the
//! encryption here provides NO protection beyond what the OS's own per-app sandbox already
//! gives a plaintext file — it is **OS-sandbox-equivalent protection, not defense-in-depth**.
//! The actual fix is a HARDWARE-backed secret (Android Keystore / iOS Keychain), where the
//! unlock material never leaves a secure enclave the app process itself cannot read raw bytes
//! from — that is what the community `tauri-plugin-keystore` (this phase's first-listed m8
//! option) would provide, and it was not vetted under this phase's time budget. Two real,
//! narrower benefits this DOES still provide over a bare plaintext file: (1) the token is never
//! trivially `grep`-able by a casual look at the data directory (it's inside a binary Stronghold
//! snapshot, not a `.json`/`.txt`), and (2) the in-memory buffer holding it while decrypted uses
//! `iota_stronghold`'s guarded-memory primitives (zeroize-on-drop, memory-dump resistance) that
//! a hand-rolled plaintext-file reader would not get for free. A true Keystore/Keychain
//! integration remains a documented follow-up, not a silently-accepted gap — see the phase
//! file's Android bring-up steps for the `android:allowBackup="false"` manifest requirement that
//! at least keeps ADB/cloud backup from exfiltrating this directory wholesale in the meantime.
use anyhow::{Context, Result};
use iota_stronghold::{KeyProvider, SnapshotPath, Stronghold};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use uuid::Uuid;

const CLIENT_PATH: &[u8] = b"haily-mobile-device";
const SNAPSHOT_FILE: &str = "mobile_vault.snapshot";
const VAULT_KEY_FILE: &str = "mobile_vault.key";
const TOKEN_KEY: &[u8] = b"device_token";
const QR_KEY: &[u8] = b"pairing_qr";

fn snapshot_path(data_dir: &Path) -> SnapshotPath {
    SnapshotPath::from_path(data_dir.join(SNAPSHOT_FILE))
}

/// Loads the persisted vault-unlock passphrase, generating and persisting a fresh
/// cryptographically-random one on first run. This file is itself unencrypted on disk, in the
/// SAME directory as the snapshot it unlocks — see the module doc's honest threat-model note for
/// why this is OS-sandbox-equivalent protection, not real defense-in-depth.
fn load_or_generate_vault_key(data_dir: &Path) -> Result<Vec<u8>> {
    let path = data_dir.join(VAULT_KEY_FILE);
    if let Ok(existing) = std::fs::read(&path) {
        if existing.len() == 32 {
            return Ok(existing);
        }
    }
    // Two v4 UUIDs concatenated = 32 CSPRNG-sourced bytes, avoiding a second `rand`-family
    // dependency purely for this one-time generation (mirrors
    // `haily-io::mobile::pairing::generate_token`'s same rationale).
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(Uuid::new_v4().as_bytes());
    key.extend_from_slice(Uuid::new_v4().as_bytes());
    std::fs::write(&path, &key).context("persisting the mobile vault's unlock key")?;
    Ok(key)
}

fn key_provider(data_dir: &Path) -> Result<KeyProvider> {
    let passphrase = load_or_generate_vault_key(data_dir)?;
    KeyProvider::with_passphrase_hashed_blake2b(passphrase)
        .map_err(|e| anyhow::anyhow!("building the vault key provider: {e:?}"))
}

/// Everything persisted about the current pairing: the device token plus enough of the
/// pairing QR to reconnect without re-scanning after an app restart.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredPairing {
    pub token: String,
    pub qr: haily_types::PairingQr,
}

/// Persists `pairing` into the vault, creating the client/snapshot on first use. Blocking
/// (file + CPU-bound crypto) — callers on the async runtime should wrap this in
/// `spawn_blocking`.
pub fn save_pairing(data_dir: &Path, pairing: &StoredPairing) -> Result<()> {
    let stronghold = Stronghold::default();
    let client = stronghold
        .create_client(CLIENT_PATH)
        .map_err(|e| anyhow::anyhow!("creating vault client: {e}"))?;
    let payload =
        serde_json::to_vec(&pairing.qr).context("serializing pairing QR for the vault")?;
    client
        .store()
        .insert(TOKEN_KEY.to_vec(), pairing.token.clone().into_bytes(), None)
        .map_err(|e| anyhow::anyhow!("writing device token to vault: {e}"))?;
    client
        .store()
        .insert(QR_KEY.to_vec(), payload, None)
        .map_err(|e| anyhow::anyhow!("writing pairing QR to vault: {e}"))?;

    let keyprovider = key_provider(data_dir)?;
    stronghold
        .commit_with_keyprovider(&snapshot_path(data_dir), &keyprovider)
        .map_err(|e| anyhow::anyhow!("committing vault snapshot: {e}"))?;
    Ok(())
}

/// Loads the persisted pairing, or `None` if this device has never paired (no snapshot file
/// yet) — never an error for the "not paired" case, only for a genuine read/decrypt failure on
/// an EXISTING snapshot (e.g. a corrupted file).
pub fn load_pairing(data_dir: &Path) -> Result<Option<StoredPairing>> {
    let path = snapshot_path(data_dir);
    if !path.exists() {
        return Ok(None);
    }
    let stronghold = Stronghold::default();
    let keyprovider = key_provider(data_dir)?;
    let client = stronghold
        .load_client_from_snapshot(CLIENT_PATH, &keyprovider, &path)
        .map_err(|e| anyhow::anyhow!("loading vault snapshot: {e}"))?;
    let Some(qr_bytes) = client
        .store()
        .get(QR_KEY)
        .map_err(|e| anyhow::anyhow!("reading pairing QR from vault: {e}"))?
    else {
        return Ok(None);
    };
    let Some(token_bytes) = client
        .store()
        .get(TOKEN_KEY)
        .map_err(|e| anyhow::anyhow!("reading device token from vault: {e}"))?
    else {
        return Ok(None);
    };
    let qr: haily_types::PairingQr =
        serde_json::from_slice(&qr_bytes).context("deserializing stored pairing QR")?;
    let token =
        String::from_utf8(token_bytes).context("stored device token was not valid UTF-8")?;
    Ok(Some(StoredPairing { token, qr }))
}

/// Forgets this pairing entirely (M5/unpair): removes the snapshot file so no trace of the
/// token/QR survives on disk. The vault-unlock key file is intentionally left in place — it
/// protects nothing on its own once its one snapshot is gone, and regenerating it needlessly
/// would only matter if some OTHER secret is ever added to this vault in the future.
pub fn clear_pairing(data_dir: &Path) -> Result<()> {
    let path = data_dir.join(SNAPSHOT_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("removing the mobile vault snapshot"),
    }
}

/// Test-only helper so the app's `data_dir` doesn't need a live Tauri path resolver in tests.
#[cfg(test)]
pub fn test_data_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("haily-mobile-vault-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create test vault dir");
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pairing() -> StoredPairing {
        StoredPairing {
            token: "test-device-token".to_string(),
            qr: haily_types::PairingQr {
                host: "127.0.0.1".to_string(),
                port: 7443,
                cert_fingerprint: "sha256:deadbeef".to_string(),
                pairing_code: "123456".to_string(),
                expires_at: "2026-07-12T00:00:00Z".to_string(),
            },
        }
    }

    #[test]
    fn no_snapshot_yet_loads_as_none() {
        let dir = test_data_dir();
        assert!(load_pairing(&dir).expect("load must not error").is_none());
    }

    #[test]
    fn save_then_load_round_trips_the_token_and_qr() {
        let dir = test_data_dir();
        let pairing = sample_pairing();
        save_pairing(&dir, &pairing).expect("save");

        let loaded = load_pairing(&dir).expect("load").expect("must be present");
        assert_eq!(loaded.token, pairing.token);
        assert_eq!(loaded.qr.host, pairing.qr.host);
        assert_eq!(loaded.qr.cert_fingerprint, pairing.qr.cert_fingerprint);
    }

    #[test]
    fn clear_pairing_removes_the_snapshot_so_load_returns_none_again() {
        let dir = test_data_dir();
        save_pairing(&dir, &sample_pairing()).expect("save");
        assert!(load_pairing(&dir).expect("load").is_some());

        clear_pairing(&dir).expect("clear");
        assert!(load_pairing(&dir).expect("load after clear").is_none());
    }

    #[test]
    fn clear_pairing_on_an_already_unpaired_device_is_not_an_error() {
        let dir = test_data_dir();
        clear_pairing(&dir).expect("clearing with nothing stored must be a no-op, not an error");
    }

    #[test]
    fn vault_key_is_generated_once_and_reused_on_a_second_call() {
        let dir = test_data_dir();
        let first = load_or_generate_vault_key(&dir).expect("first generate");
        let second = load_or_generate_vault_key(&dir).expect("second load");
        assert_eq!(
            first, second,
            "must reuse the persisted key, not regenerate"
        );
        assert_eq!(first.len(), 32);
    }
}
