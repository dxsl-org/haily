//! Per-frame authorization checks for an already-authenticated device connection (red team
//! m1/m2/m3). Split out of `server.rs` so the decision logic is unit-testable without a live
//! WebSocket.
use dashmap::DashMap;
use haily_types::MobileApprovalPolicy;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Red team M1's server-side enforcement point: whether a mobile `Approve` is honored.
///
/// `reversible` is the SAME flag `ResponseChunk::ToolApprovalRequest` already carries — `false`
/// means the underlying tool is genuinely `High`/`IrreversibleWrite` on its own merits (every
/// `ToolApprovalRequest` that reaches a client is, by construction, at least one of those, or a
/// cap-escalated `ReversibleWrite` marked `reversible: true`). This is the seam actually
/// available without extending `ApprovalResolver`'s trait surface (which lives in
/// `haily-core::approval`, outside this phase's file ownership — see the phase's Deviation
/// Log) to carry a `RiskTier` the mobile adapter could otherwise consult directly.
pub fn approval_allowed(
    policy: MobileApprovalPolicy,
    reversible: bool,
    biometric_ok: bool,
    approved: bool,
) -> bool {
    if !approved {
        return false;
    }
    match policy {
        MobileApprovalPolicy::Allow => true,
        MobileApprovalPolicy::BiometricRequired => reversible || biometric_ok,
        MobileApprovalPolicy::DenyIrreversible => reversible,
    }
}

/// Claim (first use) or verify (subsequent use) that `session_id` belongs to `device_id` (red
/// team m1 — enforced on EVERY session-scoped frame, not only `Approve`). Returns `false` when
/// the session is already owned by a DIFFERENT device — the caller must reject the frame with
/// `MobileError::SessionUnknown` rather than silently reassigning ownership.
pub fn claim_or_verify_session(
    session_owner: &DashMap<Uuid, Uuid>,
    session_id: Uuid,
    device_id: Uuid,
) -> bool {
    *session_owner.entry(session_id).or_insert(device_id) == device_id
}

/// Simple per-device sliding-window rate limiter (red team m2). One instance per connection —
/// a device only ever has one live connection at a time in practice (a reconnect replaces the
/// writer's attached socket), so this does not need to be shared across connections.
pub struct RateLimiter {
    limit_per_minute: u32,
    count: u32,
    window_start: Instant,
}

impl RateLimiter {
    pub fn new(limit_per_minute: u32) -> Self {
        Self {
            limit_per_minute,
            count: 0,
            window_start: Instant::now(),
        }
    }

    /// Returns `true` if this frame is within the allowed rate, `false` if it must be dropped.
    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start) > Duration::from_secs(60) {
            self.window_start = now;
            self.count = 0;
        }
        self.count += 1;
        self.count <= self.limit_per_minute
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_or_verify_session_first_use_claims_it() {
        let map = DashMap::new();
        let device = Uuid::new_v4();
        let session = Uuid::new_v4();
        assert!(claim_or_verify_session(&map, session, device));
        assert_eq!(*map.get(&session).unwrap(), device);
    }

    #[test]
    fn claim_or_verify_session_rejects_a_different_device() {
        let map = DashMap::new();
        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let session = Uuid::new_v4();
        assert!(claim_or_verify_session(&map, session, owner));
        assert!(!claim_or_verify_session(&map, session, intruder));
        // Ownership must be unchanged after the rejected attempt.
        assert_eq!(*map.get(&session).unwrap(), owner);
    }

    #[test]
    fn claim_or_verify_session_allows_the_same_device_repeatedly() {
        let map = DashMap::new();
        let device = Uuid::new_v4();
        let session = Uuid::new_v4();
        assert!(claim_or_verify_session(&map, session, device));
        assert!(claim_or_verify_session(&map, session, device));
        assert!(claim_or_verify_session(&map, session, device));
    }

    #[test]
    fn rate_limiter_allows_up_to_the_cap_then_denies() {
        let mut limiter = RateLimiter::new(3);
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(
            !limiter.allow(),
            "4th frame within the same window must be denied"
        );
    }

    #[test]
    fn deny_always_wins_regardless_of_policy() {
        for policy in [
            MobileApprovalPolicy::Allow,
            MobileApprovalPolicy::BiometricRequired,
            MobileApprovalPolicy::DenyIrreversible,
        ] {
            assert!(!approval_allowed(policy, true, true, false));
        }
    }

    #[test]
    fn allow_policy_honors_any_approve() {
        assert!(approval_allowed(
            MobileApprovalPolicy::Allow,
            false,
            false,
            true
        ));
    }

    #[test]
    fn biometric_required_denies_irreversible_without_biometric() {
        assert!(!approval_allowed(
            MobileApprovalPolicy::BiometricRequired,
            false,
            false,
            true
        ));
        assert!(approval_allowed(
            MobileApprovalPolicy::BiometricRequired,
            false,
            true,
            true
        ));
        // A cap-escalated reversible call never needed the extra gate in the first place.
        assert!(approval_allowed(
            MobileApprovalPolicy::BiometricRequired,
            true,
            false,
            true
        ));
    }

    #[test]
    fn deny_irreversible_never_honors_an_irreversible_approve_from_mobile() {
        assert!(!approval_allowed(
            MobileApprovalPolicy::DenyIrreversible,
            false,
            true,
            true
        ));
        assert!(approval_allowed(
            MobileApprovalPolicy::DenyIrreversible,
            true,
            true,
            true
        ));
    }
}
