//! Pairing code lifecycle (red team M4/m6) — single-use, short-TTL codes gated on an
//! out-of-band desktop confirm before any device token is ever minted.
//!
//! DESIGN DECISION (not verbatim in the spec, logged in the phase's Deviation Log): two
//! confirm modes, both satisfying M4's "a photographed QR alone must never enroll a device":
//! - `pre_approved` (headless `haily pair`): the code cannot exist until an operator with
//!   terminal access to the trusted desktop explicitly ran the minting command — that act
//!   itself IS the out-of-band confirm, so a matching `POST /pair` within the TTL redeems
//!   immediately, no second wait.
//! - interactive (GUI "Add Device", P2b): minting the code is casual (a visible button), so
//!   redemption creates a pending confirm a human must separately approve/deny — mirrors
//!   `haily-core::approval::ApprovalBroker`'s register/await/timeout-deny shape exactly.
use dashmap::DashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Single-use pairing code TTL (red team M4: shortened from a researcher-proposed 5min).
pub const PAIRING_CODE_TTL: Duration = Duration::from_secs(120);
/// Upper bound an interactive (non-pre-approved) confirm wait can run before timing out
/// deny — mirrors `ApprovalBroker::APPROVAL_TIMEOUT`'s deny-by-default safety net.
const MAX_CONFIRM_WAIT: Duration = Duration::from_secs(120);
/// Per-IP `POST /pair` attempts allowed per rolling minute before `PairingRateLimited`.
const RATE_LIMIT_PER_MINUTE: u32 = 5;
/// Sweep `rate_limit` of stale IP entries once it grows past this many unique sources (review
/// finding 6a) — otherwise an entry is written once per unique source IP ever seen and never
/// removed, growing unbounded over the life of the process.
const RATE_LIMIT_SWEEP_THRESHOLD: usize = 256;
/// An entry idle longer than this (well past the 60s rolling window used for the limit itself)
/// is swept — generous margin so a sweep can never race a legitimately still-active window.
const RATE_LIMIT_ENTRY_TTL: Duration = Duration::from_secs(300);

pub enum RedeemOutcome {
    /// Freshly confirmed — the caller must mint a device/token and report it back via
    /// [`PairingService::record_issued`] so a same-code retry (m6) replays the SAME
    /// credentials instead of minting a second device row.
    Confirmed,
    /// m6: the SAME code was already confirmed-and-issued earlier in its TTL (a dropped-ack
    /// retry) — the caller returns these EXACT credentials again, no new device row.
    AlreadyIssued {
        device_id: Uuid,
        token: String,
    },
    Denied,
    Expired,
    /// Unknown code — never issued, or its TTL already elapsed and was reaped.
    Invalid,
    RateLimited,
}

enum CodeState {
    AwaitingConfirm,
    /// A confirm decision already reached this code but no device has been minted for it yet
    /// (the brief window between `redeem` returning `Confirmed` and the caller invoking
    /// [`PairingService::record_issued`]) — a SECOND concurrent redeem in this window is
    /// treated as a fresh `Confirmed` too (rare race, mint is idempotent enough at the DB
    /// layer that a duplicate row is the caller's concern, not this state machine's).
    Resolved(bool),
    /// Terminal state: a device was minted for this code — a subsequent redeem within the TTL
    /// replays these exact credentials ONLY if `device_name` matches the value recorded at
    /// issuance (m6 dropped-ack retry); a DIFFERENT name is rejected as `Invalid`, not replayed
    /// (review HIGH: without this check, anyone who captures the code before its 120s TTL
    /// elapses — a photographed QR, a shoulder-surfed `haily pair` code — could redeem it a
    /// second time under a different device name and receive the ALREADY-ISSUED device's live
    /// token, bypassing M4's out-of-band confirm entirely on the second call). `device_name` is
    /// client-supplied and guessable, so this is defense-in-depth aligning behavior with
    /// `docs/mobile-protocol.md`'s stated contract, not a cryptographic control.
    Issued {
        device_id: Uuid,
        token: String,
        device_name: String,
    },
}

struct CodeEntry {
    device_name_hint: Option<String>,
    minted_at: Instant,
    pre_approved: bool,
    state: CodeState,
    waiters: Vec<oneshot::Sender<bool>>,
}

/// A confirm request waiting for a human decision — the OOB seam a future consumer (P2b's GUI
/// dialog) resolves via [`PairingService::confirm`]. Exposed the same shape as
/// `haily-core::approval::PendingApproval` for the same reason: a reconcile-style read a UI
/// polls, not a push.
pub struct PendingConfirm {
    pub code: String,
    pub device_name: String,
}

pub struct PairingService {
    codes: DashMap<String, CodeEntry>,
    rate_limit: DashMap<IpAddr, (u32, Instant)>,
    now: fn() -> Instant,
}

impl PairingService {
    pub fn new() -> Self {
        Self {
            codes: DashMap::new(),
            rate_limit: DashMap::new(),
            now: Instant::now,
        }
    }

    /// Injectable clock for deterministic TTL tests (this phase's own tests, and P6's
    /// integration tests — kept `pub`, not `#[cfg(test)]`, since an external test binary
    /// cannot see this crate's unit-test cfg).
    pub fn with_clock(now: fn() -> Instant) -> Self {
        Self {
            codes: DashMap::new(),
            rate_limit: DashMap::new(),
            now,
        }
    }

    /// Mint a fresh single-use code. `pre_approved` selects the confirm mode (see module doc).
    pub fn mint_code(&self, device_name_hint: Option<String>, pre_approved: bool) -> String {
        let code = generate_pairing_code();
        self.codes.insert(
            code.clone(),
            CodeEntry {
                device_name_hint,
                minted_at: (self.now)(),
                pre_approved,
                state: CodeState::AwaitingConfirm,
                waiters: Vec::new(),
            },
        );
        code
    }

    fn is_rate_limited(&self, source: IpAddr) -> bool {
        let now = (self.now)();
        let mut entry = self.rate_limit.entry(source).or_insert((0, now));
        if now.duration_since(entry.1) > Duration::from_secs(60) {
            *entry = (0, now);
        }
        entry.0 += 1;
        let limited = entry.0 > RATE_LIMIT_PER_MINUTE;
        drop(entry); // release the shard lock before the sweep below may touch it
        self.sweep_stale_rate_limit_entries(now);
        limited
    }

    /// Evict rate-limit entries idle past [`RATE_LIMIT_ENTRY_TTL`] once the map grows past
    /// [`RATE_LIMIT_SWEEP_THRESHOLD`] (review finding 6a) — otherwise one entry per unique
    /// source IP ever seen accumulates forever.
    fn sweep_stale_rate_limit_entries(&self, now: Instant) {
        if self.rate_limit.len() <= RATE_LIMIT_SWEEP_THRESHOLD {
            return;
        }
        self.rate_limit.retain(|_, (_, window_start)| {
            now.duration_since(*window_start) < RATE_LIMIT_ENTRY_TTL
        });
    }

    /// `POST /pair`'s core: validate + (if needed) await confirm. Idempotent within TTL (m6):
    /// a code already `Issued` (a prior successful redeem minted a device) replays those SAME
    /// credentials rather than re-running the confirm gate, rate limit check aside.
    pub async fn redeem(&self, code: &str, device_name: &str, source: IpAddr) -> RedeemOutcome {
        if self.is_rate_limited(source) {
            return RedeemOutcome::RateLimited;
        }

        enum Snapshot {
            Issued(Uuid, String, String),
            Resolved(bool),
            Fresh,
        }
        let (snapshot, pre_approved, expired) = {
            let Some(entry) = self.codes.get(code) else {
                return RedeemOutcome::Invalid;
            };
            let expired = (self.now)().duration_since(entry.minted_at) > PAIRING_CODE_TTL;
            let snap = match &entry.state {
                CodeState::Issued {
                    device_id,
                    token,
                    device_name: issued_name,
                } => Snapshot::Issued(*device_id, token.clone(), issued_name.clone()),
                CodeState::Resolved(approved) => Snapshot::Resolved(*approved),
                CodeState::AwaitingConfirm => Snapshot::Fresh,
            };
            (snap, entry.pre_approved, expired)
        };
        // An already-Issued code replays regardless of TTL elapsing between issuance and this
        // retry — the device is already real; only an UN-issued code is reaped on expiry.
        // BUT only for the SAME device_name recorded at issuance — a different name on a still-
        // live code is a second party racing the code, not the legitimate dropped-ack retry m6
        // exists for, and must be rejected rather than handed the live token.
        if let Snapshot::Issued(device_id, token, issued_name) = snapshot {
            return if device_name == issued_name {
                RedeemOutcome::AlreadyIssued { device_id, token }
            } else {
                RedeemOutcome::Invalid
            };
        }
        if expired {
            self.codes.remove(code);
            return RedeemOutcome::Expired;
        }
        if let Snapshot::Resolved(approved) = snapshot {
            return if approved {
                RedeemOutcome::Confirmed
            } else {
                RedeemOutcome::Denied
            };
        }

        if pre_approved {
            self.resolve(code, true);
            return RedeemOutcome::Confirmed;
        }

        let rx = {
            let Some(mut entry) = self.codes.get_mut(code) else {
                return RedeemOutcome::Invalid;
            };
            let (tx, rx) = oneshot::channel();
            entry.waiters.push(tx);
            rx
        };

        match tokio::time::timeout(MAX_CONFIRM_WAIT, rx).await {
            Ok(Ok(approved)) => {
                if approved {
                    RedeemOutcome::Confirmed
                } else {
                    RedeemOutcome::Denied
                }
            }
            _ => RedeemOutcome::Denied, // timeout or sender dropped — deny-by-default
        }
    }

    /// Resolve a pending confirm (the future GUI dialog's "Approve"/"Deny" action). `true` if a
    /// matching code was found; `false` for an unknown/already-resolved code.
    pub fn confirm(&self, code: &str, approved: bool) -> bool {
        self.resolve(code, approved)
    }

    /// Record that `code`'s `Confirmed` outcome was minted into a real device — a subsequent
    /// `redeem` of this code within its TTL replays these credentials (m6) ONLY when its
    /// `device_name` matches `device_name` here; a different name is rejected instead (see the
    /// `Issued` variant's doc). A no-op if `code` is unknown (already reaped) or already
    /// `Issued` (the caller should not double-mint in the first place).
    pub fn record_issued(&self, code: &str, device_id: Uuid, token: String, device_name: String) {
        if let Some(mut entry) = self.codes.get_mut(code) {
            if !matches!(entry.state, CodeState::Issued { .. }) {
                entry.state = CodeState::Issued {
                    device_id,
                    token,
                    device_name,
                };
            }
        }
    }

    fn resolve(&self, code: &str, approved: bool) -> bool {
        let Some(mut entry) = self.codes.get_mut(code) else {
            return false;
        };
        if !matches!(entry.state, CodeState::AwaitingConfirm) {
            return false;
        }
        entry.state = CodeState::Resolved(approved);
        for tx in entry.waiters.drain(..) {
            let _ = tx.send(approved);
        }
        true
    }

    /// Every code still awaiting a human decision — the future GUI dialog's reconcile source.
    pub fn pending_confirms(&self) -> Vec<PendingConfirm> {
        self.codes
            .iter()
            .filter(|e| matches!(e.state, CodeState::AwaitingConfirm) && !e.pre_approved)
            .map(|e| PendingConfirm {
                code: e.key().clone(),
                device_name: e
                    .device_name_hint
                    .clone()
                    .unwrap_or_else(|| "unknown device".to_string()),
            })
            .collect()
    }
}

impl Default for PairingService {
    fn default() -> Self {
        Self::new()
    }
}

/// A high-entropy bearer token — two concatenated UUIDv4s (256 bits) rather than pulling in a
/// separate `rand` dependency; `uuid`'s v4 generator is already backed by a CSPRNG.
pub fn generate_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// A 6-digit human-typeable pairing code, derived from a UUIDv4's entropy.
fn generate_pairing_code() -> String {
    let bytes = Uuid::new_v4().into_bytes();
    let n = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % 1_000_000;
    format!("{n:06}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn ip() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
    }

    #[tokio::test]
    async fn pre_approved_code_redeems_immediately_without_a_confirm_wait() {
        let svc = PairingService::new();
        let code = svc.mint_code(Some("Phone".into()), true);
        let outcome = svc.redeem(&code, "Phone", ip()).await;
        assert!(matches!(outcome, RedeemOutcome::Confirmed));
    }

    #[tokio::test]
    async fn interactive_code_waits_for_an_explicit_confirm() {
        let svc = std::sync::Arc::new(PairingService::new());
        let code = svc.mint_code(Some("Phone".into()), false);

        let svc2 = svc.clone();
        let code2 = code.clone();
        let redeem_fut = tokio::spawn(async move { svc2.redeem(&code2, "Phone", ip()).await });

        // Give redeem() time to register its waiter before confirming.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(svc.confirm(&code, true));

        let outcome = redeem_fut.await.unwrap();
        assert!(matches!(outcome, RedeemOutcome::Confirmed));
    }

    #[tokio::test]
    async fn interactive_code_denied_rejects_redeem() {
        let svc = std::sync::Arc::new(PairingService::new());
        let code = svc.mint_code(None, false);

        let svc2 = svc.clone();
        let code2 = code.clone();
        let redeem_fut = tokio::spawn(async move { svc2.redeem(&code2, "Phone", ip()).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(svc.confirm(&code, false));

        assert!(matches!(redeem_fut.await.unwrap(), RedeemOutcome::Denied));
    }

    #[tokio::test]
    async fn unknown_code_is_invalid() {
        let svc = PairingService::new();
        assert!(matches!(
            svc.redeem("000000", "x", ip()).await,
            RedeemOutcome::Invalid
        ));
    }

    /// m6: redeeming the SAME already-confirmed-AND-ISSUED code again (a dropped-ack retry)
    /// must replay the SAME credentials without re-running the confirm gate or minting twice.
    #[tokio::test]
    async fn idempotent_redemption_within_ttl_returns_the_same_outcome() {
        let svc = PairingService::new();
        let code = svc.mint_code(None, true);
        assert!(matches!(
            svc.redeem(&code, "Phone", ip()).await,
            RedeemOutcome::Confirmed
        ));

        let device_id = Uuid::new_v4();
        let token = generate_token();
        svc.record_issued(&code, device_id, token.clone(), "Phone".to_string());

        match svc.redeem(&code, "Phone", ip()).await {
            RedeemOutcome::AlreadyIssued {
                device_id: replayed_id,
                token: replayed_token,
            } => {
                assert_eq!(replayed_id, device_id);
                assert_eq!(replayed_token, token);
            }
            _ => panic!("expected AlreadyIssued on a same-code retry"),
        }
    }

    #[test]
    fn record_issued_on_an_unknown_code_is_a_harmless_no_op() {
        let svc = PairingService::new();
        svc.record_issued(
            "000000",
            Uuid::new_v4(),
            "tok".to_string(),
            "Phone".to_string(),
        );
    }

    #[tokio::test]
    async fn expired_code_is_reaped_and_reported_expired() {
        let svc = PairingService::with_clock(Instant::now);
        let code = svc.mint_code(None, true);
        // Directly age the entry past the TTL rather than sleeping in a unit test — the
        // injectable-clock constructor above is what P6's integration tests use instead;
        // this unit test only needs private-field access, available since `tests` is a
        // descendant module of `pairing`.
        if let Some(mut entry) = svc.codes.get_mut(&code) {
            entry.minted_at = Instant::now() - PAIRING_CODE_TTL - Duration::from_secs(1);
        }
        let outcome = svc.redeem(&code, "Phone", ip()).await;
        assert!(matches!(outcome, RedeemOutcome::Expired));
    }

    #[tokio::test]
    async fn rate_limit_kicks_in_after_the_per_minute_cap() {
        let svc = PairingService::new();
        for _ in 0..RATE_LIMIT_PER_MINUTE {
            let _ = svc.redeem("nope", "x", ip()).await;
        }
        assert!(matches!(
            svc.redeem("nope", "x", ip()).await,
            RedeemOutcome::RateLimited
        ));
    }

    #[test]
    fn generate_token_is_high_entropy_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64); // two UUIDv4 simple-form hex strings, 32 chars each
    }

    #[test]
    fn generate_pairing_code_is_six_digits() {
        let code = generate_pairing_code();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[tokio::test]
    async fn pending_confirms_lists_only_interactive_unresolved_codes() {
        let svc = PairingService::new();
        let interactive = svc.mint_code(Some("Phone A".into()), false);
        let _pre_approved = svc.mint_code(Some("Phone B".into()), true);

        let pending = svc.pending_confirms();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].code, interactive);
        assert_eq!(pending[0].device_name, "Phone A");
    }

    /// Review finding 6a: once the rate-limit map grows past the sweep threshold, a stale
    /// (long-idle) entry is evicted rather than accumulating forever.
    #[tokio::test]
    async fn rate_limit_sweeps_stale_entries_past_the_threshold() {
        let svc = PairingService::new();
        let stale_ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        // Seed a stale entry directly, well past `RATE_LIMIT_ENTRY_TTL`.
        svc.rate_limit.insert(
            stale_ip,
            (
                1,
                Instant::now() - RATE_LIMIT_ENTRY_TTL - Duration::from_secs(1),
            ),
        );
        // Pad past the sweep threshold with fresh, distinct source IPs.
        for i in 0..(RATE_LIMIT_SWEEP_THRESHOLD as u32 + 1) {
            let ip = IpAddr::V4(Ipv4Addr::from(i.to_be_bytes()));
            let _ = svc.redeem("000000", "x", ip).await;
        }

        assert!(
            !svc.rate_limit.contains_key(&stale_ip),
            "a stale entry must be evicted once the map is large enough for the sweep to run"
        );
    }
}
