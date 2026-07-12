//! Thin app-layer surface for the desktop GUI's mobile pairing/devices panel (Mobile
//! Thin-Client plan phase 2b). Mirrors `connector_config`'s split: this module is pure
//! delegation onto P2a's public `haily_io::mobile` API + `haily_db::queries::devices` — no
//! new persistence, no changes to `haily-io/src/mobile/*.rs` (out of this phase's ownership).
//!
//! **Pairing confirm (M4) is POLL-based, not push (Deviation Log entry):** P2a exposes
//! `MobileAdapter::pending_pair_confirms()` as a plain read, with no event channel for "a new
//! pairing request just arrived" — none of the existing Tauri event bridges (chunk/work-items/
//! proactive/run-events) carry pairing state, and adding one would mean editing P2a's `mod.rs`,
//! which this phase must not touch. The GUI panel instead polls this on a short interval while
//! open (mirrors `list_approvals`' reconcile-only contract) — the confirm gate itself is fully
//! real end-to-end (a phone cannot obtain a token until `confirm_pair` is called), only the
//! desktop's *notice* that a request is waiting is poll-latency-bounded rather than instant.
use anyhow::{ensure, Context, Result};
use haily_db::queries::devices;
use haily_db::DbHandle;
use haily_io::mobile::{bind, pairing, tls, MobileAdapter, MobileServerConfig};
use haily_types::PairingQr;
use serde::Serialize;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

/// Bound on the loopback liveness probe in [`mobile_status`] — generous enough for a local
/// TCP handshake, tight enough that a hung/firewalled port never stalls the panel's load.
const LOOPBACK_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// Read-only snapshot for the panel's status banners (running / degraded / tailnet-absent,
/// red team M2/M11/"Tailscale prerequisite").
#[derive(Debug, Clone, Serialize)]
pub struct MobileStatusView {
    pub enabled: bool,
    /// Best-effort "is something actually listening on loopback" liveness check — see
    /// [`mobile_status`]'s doc for why this is the only observable signal available without
    /// editing P2a's `server.rs` (`MobileAdapter::start` is deliberately fire-and-forget, M11).
    pub running: bool,
    /// Whether a tailnet (Tailscale CGNAT) interface is currently selectable — `false` drives
    /// the "Tailscale is a prerequisite" first-run banner.
    pub tailnet_present: bool,
    pub lan_opt_in: bool,
    pub port: u16,
}

/// Best-effort liveness probe. A bind failure inside `MobileAdapter::start`'s spawned task is
/// intentionally invisible to its caller (M11: never abort GUI boot over the remote channel),
/// so there is no other in-process signal for "did the server actually come up" short of this
/// external connect attempt — the same technique `haily pair`'s human operator would use by
/// eye (does the client connect), just automated for the panel.
async fn probe_loopback(port: u16) -> bool {
    tokio::time::timeout(
        LOOPBACK_PROBE_TIMEOUT,
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// Status for the panel's banners. `running` is only probed when `enabled` — a disabled
/// config never even attempts the connect (nothing should be listening either way).
pub async fn mobile_status(cfg: &MobileServerConfig) -> MobileStatusView {
    let interfaces = bind::enumerate_interfaces();
    // `lan_opt_in: false` here on purpose: with LAN opt-in off, `select_bind_addrs` only ever
    // adds a tailnet address beyond loopback, so "any non-loopback entry" is exactly the
    // tailnet-presence signal the banner needs, independent of the user's own LAN opt-in choice.
    let tailnet_present = bind::select_bind_addrs(&interfaces, false, cfg.port)
        .iter()
        .any(|a| !a.ip().is_loopback());
    let running = cfg.enabled && probe_loopback(cfg.port).await;
    MobileStatusView {
        enabled: cfg.enabled,
        running,
        tailnet_present,
        lan_opt_in: cfg.lan_opt_in,
        port: cfg.port,
    }
}

/// Mint a fresh pairing code and build the QR payload the phone scans. Uses the INTERACTIVE
/// confirm mode (`pre_approved: false`) — minting from a GUI button is casual, unlike `haily
/// pair`'s terminal-access ceremony, so every redemption must wait on [`confirm_pair`] (M4).
///
/// # Errors
/// Returns an error if the TLS identity cannot be loaded/generated, or if the TTL cannot be
/// converted to a `chrono::Duration` (never in practice — see `pairing::PAIRING_CODE_TTL`).
pub async fn pairing_qr(
    adapter: &MobileAdapter,
    data_dir: &Path,
    cfg: &MobileServerConfig,
    device_name_hint: Option<String>,
) -> Result<PairingQr> {
    let code = adapter.mint_pairing_code(device_name_hint, false);
    let cert = tls::load_or_generate(data_dir)?;
    let interfaces = bind::enumerate_interfaces();
    let host = bind::select_bind_addrs(&interfaces, false, cfg.port)
        .into_iter()
        .find(|a| !a.ip().is_loopback())
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let expires_at =
        (chrono::Utc::now() + chrono::Duration::from_std(pairing::PAIRING_CODE_TTL)?).to_rfc3339();
    Ok(PairingQr {
        host,
        port: cfg.port,
        cert_fingerprint: cert.fingerprint,
        pairing_code: code,
        expires_at,
    })
}

/// One pairing request still awaiting the desktop's OOB decision (M4).
#[derive(Debug, Clone, Serialize)]
pub struct PendingPairView {
    pub code: String,
    pub device_name: String,
}

/// Every interactive pairing code still awaiting a confirm decision — the panel's poll source
/// (see the module doc for why this is polled, not pushed).
pub fn pending_pairs(adapter: &MobileAdapter) -> Vec<PendingPairView> {
    adapter
        .pending_pair_confirms()
        .into_iter()
        .map(|p| PendingPairView {
            code: p.code,
            device_name: p.device_name,
        })
        .collect()
}

/// Resolve a pending pairing confirm — the panel's Approve/Deny action. Returns `false` for an
/// unknown/already-resolved code (nothing to do), mirroring `resolveApproval`'s convention.
pub fn confirm_pair(adapter: &MobileAdapter, code: &str, approve: bool) -> bool {
    adapter.confirm_pairing(code, approve)
}

/// One paired device row for the Devices panel.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceView {
    pub device_id: String,
    pub device_name: String,
    pub created_at: String,
    pub last_seen_at: Option<String>,
}

/// Every non-revoked device, most-recently-paired first.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_devices(db: &DbHandle) -> Result<Vec<DeviceView>> {
    let rows = devices::list_active(db).await?;
    Ok(rows
        .into_iter()
        .map(|r| DeviceView {
            device_id: r.device_id,
            device_name: r.device_name,
            created_at: r.created_at,
            last_seen_at: r.last_seen_at,
        })
        .collect())
}

/// Revoke a paired device: soft-revokes the persisted row AND ends its live connection
/// immediately (`MobileAdapter::disconnect_device`) — both halves are needed, mirroring
/// `server.rs`'s own `revoke_poll_loop` comment: the DB write alone would leave an
/// already-open socket live for up to `REVOKE_POLL_INTERVAL` longer.
///
/// # Errors
/// Returns an error if the DB revoke write fails.
pub async fn revoke_device(db: &DbHandle, adapter: &MobileAdapter, device_id: Uuid) -> Result<()> {
    devices::revoke(db, device_id).await?;
    adapter.disconnect_device(device_id);
    Ok(())
}

/// Force TLS identity regeneration — an explicit IDENTITY ROTATION (m5: "cert lifecycle is
/// first-class"), NOT an access revocation: every already-paired device's row stays intact and
/// keeps working over loopback/tailnet (no TLS there); only its pinned LAN-direct (`wss://`)
/// fingerprint goes stale until it re-pairs. Use [`revoke_device`] to actually lock a device
/// out. Deletes the persisted cert/key files (if present) so the next
/// [`tls::load_or_generate`] call regenerates rather than reusing them — that function itself
/// only regenerates on a missing/corrupt file, by design (its own doc comment), so forcing it
/// from here is the intended mechanism rather than a workaround. Also forces every currently
/// paired device's live connection to reconnect (review finding 3): a LAN-connected phone must
/// attempt a fresh TLS handshake against the NEW cert rather than keep riding an already-
/// established connection indefinitely — `disconnect_device` self-heals on that device's next
/// successful reconnect (the WS upgrade handler clears `revoked_cache` after a fresh DB-backed
/// auth success), so this is a forced reconnect, not a revoke.
///
/// # Errors
/// Returns an error if a stale cert/key file exists but cannot be removed (review finding 2 —
/// a swallowed removal failure would otherwise leave the OLD identity in place while reporting
/// success), if regeneration/persisting the new identity fails, or if the resulting fingerprint
/// is unexpectedly unchanged (the same failure mode, caught defensively after the fact).
pub async fn regenerate_cert(
    db: &DbHandle,
    adapter: &MobileAdapter,
    data_dir: &Path,
) -> Result<String> {
    let (cert_path, key_path) = tls::cert_paths(data_dir);

    // Capture the current fingerprint ONLY if a cert already exists — `load_or_generate` on an
    // already-missing file would itself generate one as a side effect, which would be wrong to
    // trigger merely for a "what's the prior value" read.
    let previous_fingerprint = if cert_path.exists() {
        tls::load_or_generate(data_dir).ok().map(|c| c.fingerprint)
    } else {
        None
    };

    remove_cert_file(&cert_path)?;
    remove_cert_file(&key_path)?;

    let regenerated = tls::load_or_generate(data_dir)?;
    if let Some(previous) = previous_fingerprint {
        ensure!(
            regenerated.fingerprint != previous,
            "certificate regeneration produced the same fingerprint as before — the old \
             cert/key files may not have actually been removed"
        );
    }

    if let Ok(devices) = devices::list_active(db).await {
        for d in devices {
            if let Ok(device_id) = Uuid::parse_str(&d.device_id) {
                adapter.disconnect_device(device_id);
            }
        }
    }

    Ok(regenerated.fingerprint)
}

/// Remove a cert/key file, treating "already absent" as success (nothing to delete) but
/// propagating every OTHER I/O error (permissions, file locked by another process) — a
/// swallowed real failure here would leave the stale file in place, and the caller would then
/// silently report the OLD fingerprint as if regeneration had succeeded (review finding 2).
fn remove_cert_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("failed to remove stale file at {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use haily_io::mobile::MobileDeviceStore;
    use std::sync::Arc;

    struct FakeDeviceStore;

    #[async_trait]
    impl MobileDeviceStore for FakeDeviceStore {
        async fn find_active_by_token_hash(&self, _token_hash: &str) -> Option<Uuid> {
            None
        }
        async fn is_revoked(&self, _device_id: Uuid) -> bool {
            true
        }
        async fn touch_last_seen(&self, _device_id: Uuid) {}
        async fn create_device(&self, _device_name: &str, _token_hash: &str) -> Option<Uuid> {
            Some(Uuid::new_v4())
        }
    }

    fn adapter(dir: &Path) -> MobileAdapter {
        MobileAdapter::new(
            MobileServerConfig::default(),
            Arc::new(FakeDeviceStore),
            dir.to_path_buf(),
        )
    }

    #[tokio::test]
    async fn mobile_status_disabled_never_reports_running() {
        let cfg = MobileServerConfig::default(); // enabled: false by default
        let status = mobile_status(&cfg).await;
        assert!(!status.enabled);
        assert!(
            !status.running,
            "a disabled config must never probe as running"
        );
    }

    #[tokio::test]
    async fn pairing_qr_builds_a_well_formed_payload() {
        let dir = tempfile::tempdir().unwrap();
        let a = adapter(dir.path());
        let cfg = MobileServerConfig::default();
        let qr = pairing_qr(&a, dir.path(), &cfg, Some("Phone".into()))
            .await
            .expect("pairing_qr must succeed");
        assert!(qr.cert_fingerprint.starts_with("sha256:"));
        assert_eq!(qr.pairing_code.len(), 6);
        assert_eq!(qr.port, cfg.port);
    }

    #[test]
    fn pending_pairs_and_confirm_pair_roundtrip_through_the_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let a = adapter(dir.path());
        let code = a.mint_pairing_code(Some("Phone".into()), false);

        let pending = pending_pairs(&a);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].code, code);
        assert_eq!(pending[0].device_name, "Phone");

        assert!(confirm_pair(&a, &code, true));
        assert!(
            pending_pairs(&a).is_empty(),
            "a resolved code must no longer be pending"
        );
    }

    #[test]
    fn confirm_pair_on_an_unknown_code_is_a_harmless_false() {
        let dir = tempfile::tempdir().unwrap();
        let a = adapter(dir.path());
        assert!(!confirm_pair(&a, "000000", true));
    }

    #[tokio::test]
    async fn regenerate_cert_produces_a_different_fingerprint_than_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        let a = adapter(dir.path());
        let first = tls::load_or_generate(dir.path()).unwrap().fingerprint;
        let second = regenerate_cert(&db, &a, dir.path())
            .await
            .expect("regenerate must succeed");
        assert_ne!(
            first, second,
            "forcing a regeneration must invalidate the previous fingerprint"
        );
    }

    #[tokio::test]
    async fn regenerate_cert_on_a_fresh_data_dir_does_not_require_a_prior_cert() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        let a = adapter(dir.path());
        // No cert has ever been generated in this dir yet — must still succeed (no prior
        // fingerprint to compare against).
        let fingerprint = regenerate_cert(&db, &a, dir.path())
            .await
            .expect("regenerate must succeed even with nothing to remove");
        assert!(fingerprint.starts_with("sha256:"));
    }

    // `MobileAdapter::disconnect_device`'s internal effects (`revoked_cache`, `session_owner`,
    // connection cancellation) live behind `pub(crate)` fields in `haily-io` — not observable
    // from this crate's test, and that per-call contract is already unit-tested in
    // `haily-io::mobile::mod::tests`. This test instead proves the INTEGRATION: regenerating
    // with paired devices on record runs the disconnect loop (`devices::list_active` +
    // `disconnect_device` per row) to completion without erroring or panicking.
    #[tokio::test]
    async fn regenerate_cert_succeeds_with_paired_devices_on_record() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        let a = adapter(dir.path());
        devices::insert(&db, Uuid::new_v4(), "Phone", &devices::hash_token("tok"))
            .await
            .unwrap();

        let fingerprint = regenerate_cert(&db, &a, dir.path())
            .await
            .expect("regenerate must succeed even while devices are paired");
        assert!(fingerprint.starts_with("sha256:"));
    }

    #[test]
    fn remove_cert_file_on_an_already_absent_path_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.der");
        assert!(remove_cert_file(&path).is_ok());
    }

    /// Review finding 2: a real removal failure (not "already absent") must propagate, never
    /// be swallowed. `remove_file` on a directory errors deterministically cross-platform —
    /// portable stand-in for "permission denied"/"file locked" without relying on OS-specific
    /// locking behavior.
    #[test]
    fn remove_cert_file_propagates_a_real_removal_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-actually-a-file");
        std::fs::create_dir(&path).unwrap();
        let err = remove_cert_file(&path).expect_err("removing a directory must error");
        assert!(err.to_string().contains("failed to remove"));
    }

    #[tokio::test]
    async fn list_devices_and_revoke_device_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        let device_id = Uuid::new_v4();
        devices::insert(&db, device_id, "Phone", &devices::hash_token("tok"))
            .await
            .unwrap();

        let listed = list_devices(&db).await.expect("list must succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].device_id, device_id.to_string());

        let a = adapter(dir.path());
        revoke_device(&db, &a, device_id)
            .await
            .expect("revoke must succeed");
        let listed = list_devices(&db).await.expect("list must succeed");
        assert!(
            listed.is_empty(),
            "a revoked device must not be listed as active"
        );
    }
}
