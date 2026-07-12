//! Self-signed TLS certificate lifecycle for the LAN-opt-in (`wss://`) listener.
//!
//! Generated once and persisted under `data_dir` so the fingerprint the pairing QR carries
//! stays stable across restarts — regenerating it invalidates every already-paired device's
//! pinned fingerprint (red team m5), so this only happens when no valid cert file exists yet
//! or the persisted bytes fail to parse (corruption), never on an ordinary boot.
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const CERT_FILE: &str = "mobile_cert.der";
const KEY_FILE: &str = "mobile_key.der";

/// A generated (or loaded) server identity: DER-encoded cert + private key, plus the
/// human/QR-facing fingerprint derived from the cert bytes.
pub struct ServerCert {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    /// `"sha256:<hex>"` — the exact form the pairing QR (`PairingQr.cert_fingerprint`) carries.
    pub fingerprint: String,
}

fn fingerprint_of(cert_der: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    format!("sha256:{:x}", hasher.finalize())
}

/// Load the persisted cert/key from `data_dir` if present and parseable; otherwise generate a
/// fresh self-signed identity and persist it. A regeneration (fresh generate, whether because
/// no file existed yet or the persisted bytes were corrupt) is logged at `warn` — the caller-
/// visible consequence is that every already-paired device's pinned fingerprint now mismatches
/// and shows the "desktop identity changed — re-pair" banner (m5), never a silent trust
/// downgrade.
///
/// # Errors
/// Returns an error if generation succeeds but persisting the new files fails (e.g. the data
/// directory is not writable) — a certificate that cannot be persisted would regenerate (and
/// invalidate every pairing) on every restart, which must fail loudly rather than silently.
pub fn load_or_generate(data_dir: &Path) -> Result<ServerCert> {
    let cert_path = data_dir.join(CERT_FILE);
    let key_path = data_dir.join(KEY_FILE);

    if let (Ok(cert_der), Ok(key_der)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        if !cert_der.is_empty() && !key_der.is_empty() {
            let fingerprint = fingerprint_of(&cert_der);
            return Ok(ServerCert {
                cert_der,
                key_der,
                fingerprint,
            });
        }
    }

    tracing::warn!(
        "mobile: no valid persisted TLS identity found — generating a new self-signed \
         certificate. Every already-paired device's pinned fingerprint will now mismatch and \
         must re-pair (red team m5) — this is expected on first boot, not on a healthy restart."
    );
    generate_and_persist(&cert_path, &key_path)
}

fn generate_and_persist(cert_path: &Path, key_path: &Path) -> Result<ServerCert> {
    let certified = rcgen::generate_simple_self_signed(vec!["haily.local".to_string()])
        .context("generating self-signed mobile-server certificate")?;
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();

    std::fs::write(cert_path, &cert_der).context("persisting mobile server certificate")?;
    std::fs::write(key_path, &key_der).context("persisting mobile server private key")?;

    let fingerprint = fingerprint_of(&cert_der);
    Ok(ServerCert {
        cert_der,
        key_der,
        fingerprint,
    })
}

/// Convenience for callers that only need the fingerprint (e.g. re-rendering the QR after the
/// server already started) without re-deriving DER paths themselves.
pub fn cert_paths(data_dir: &Path) -> (PathBuf, PathBuf) {
    (data_dir.join(CERT_FILE), data_dir.join(KEY_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_and_persists_a_loadable_identity() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_generate(dir.path()).expect("generate");
        assert!(first.fingerprint.starts_with("sha256:"));
        assert!(!first.cert_der.is_empty());
        assert!(!first.key_der.is_empty());

        let (cert_path, key_path) = cert_paths(dir.path());
        assert!(cert_path.exists());
        assert!(key_path.exists());
    }

    #[test]
    fn a_second_load_reuses_the_persisted_identity_not_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_generate(dir.path()).expect("generate");
        let second = load_or_generate(dir.path()).expect("reload");
        assert_eq!(
            first.fingerprint, second.fingerprint,
            "must reuse the persisted cert, not regenerate"
        );
        assert_eq!(first.cert_der, second.cert_der);
    }

    #[test]
    fn corrupt_persisted_cert_triggers_regeneration_rather_than_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path) = cert_paths(dir.path());
        std::fs::write(&cert_path, b"").unwrap(); // empty = treated as absent
        std::fs::write(&key_path, b"").unwrap();

        let result = load_or_generate(dir.path());
        assert!(
            result.is_ok(),
            "corrupt/empty persisted files must regenerate, not error"
        );
    }
}
