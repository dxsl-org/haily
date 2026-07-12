//! ACP session registry (Sub-Agent + Skill Architecture phase 12).
//!
//! Maps the stable, public ACP `sessionId` (the sole auth handle the editor holds) onto
//! Haily's internal session `Uuid` (which the orchestrator + `sessions` storage key on).
//! Keeping the two distinct is deliberate: the ACP id is a durable public handle while
//! internal turn ids rotate. Also tracks each session's [`SessionMode`] (the auto-approve
//! policy) and the set of tool approvals currently awaiting the editor, so a
//! `session/cancel` can DENY every in-flight approval for that session (fail-safe).

use super::protocol::SessionMode;
use dashmap::DashMap;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Per-session state behind the ACP id.
#[derive(Debug, Clone)]
struct SessionState {
    haily_id: Uuid,
    mode: SessionMode,
    /// The target repo root the editor opened (ACP `cwd`), used to resolve a relative write
    /// path to an absolute one for the edit-diff preview. `None` until `session/new` supplies it.
    cwd: Option<String>,
    /// Approvals surfaced to the editor and not yet resolved, each with its own cancel token.
    /// `session/cancel` fires every token so the awaiting `request_permission` returns
    /// immediately as a DENY — no 60s timeout wait.
    pending: HashMap<Uuid, CancellationToken>,
}

/// Thread-safe registry of live ACP sessions. Cheap to clone (all state is Arc'd inside the
/// `DashMap`s), so the adapter and its read loop can share one.
#[derive(Default)]
pub struct AcpSessions {
    by_acp: DashMap<String, SessionState>,
    /// Reverse index so `deliver(haily_id, chunk)` (called by the orchestrator with the
    /// internal id) can resolve the public ACP id to stamp on a `session/update`.
    haily_to_acp: DashMap<Uuid, String>,
}

impl AcpSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a brand-new session: a fresh public ACP id + a fresh internal Haily id.
    pub fn new_session(&self) -> (String, Uuid) {
        let acp_id = Uuid::new_v4().to_string();
        let haily_id = Uuid::new_v4();
        self.register(acp_id.clone(), haily_id);
        (acp_id, haily_id)
    }

    /// Re-attach an existing ACP id (client-supplied on `session/load`/`resume`) to a Haily
    /// id. If the ACP id is unknown, a fresh Haily id is minted and the mapping recorded so
    /// the transcript can still be replayed and new turns routed. Returns the Haily id.
    pub fn attach(&self, acp_id: &str, haily_id: Option<Uuid>) -> Uuid {
        if let Some(existing) = self.by_acp.get(acp_id) {
            return existing.haily_id;
        }
        let haily_id = haily_id.unwrap_or_else(Uuid::new_v4);
        self.register(acp_id.to_string(), haily_id);
        haily_id
    }

    /// Fork a session: a NEW public ACP id bound to a NEW internal id. The caller replays the
    /// parent's transcript into the fork; from there the two diverge independently.
    pub fn fork(&self, _parent_acp_id: &str) -> (String, Uuid) {
        self.new_session()
    }

    fn register(&self, acp_id: String, haily_id: Uuid) {
        self.haily_to_acp.insert(haily_id, acp_id.clone());
        self.by_acp.insert(
            acp_id,
            SessionState { haily_id, mode: SessionMode::Default, cwd: None, pending: HashMap::new() },
        );
    }

    pub fn set_cwd(&self, acp_id: &str, cwd: Option<String>) {
        if let Some(mut s) = self.by_acp.get_mut(acp_id) {
            s.cwd = cwd;
        }
    }

    /// The target-repo root for the session behind the internal Haily id, if one was set —
    /// used to resolve a relative write path for the edit-diff preview.
    pub fn cwd_for_haily(&self, haily_id: &Uuid) -> Option<String> {
        self.acp_id(haily_id)
            .and_then(|a| self.by_acp.get(&a).and_then(|s| s.cwd.clone()))
    }

    pub fn haily_id(&self, acp_id: &str) -> Option<Uuid> {
        self.by_acp.get(acp_id).map(|s| s.haily_id)
    }

    pub fn acp_id(&self, haily_id: &Uuid) -> Option<String> {
        self.haily_to_acp.get(haily_id).map(|s| s.clone())
    }

    pub fn mode(&self, acp_id: &str) -> SessionMode {
        self.by_acp.get(acp_id).map(|s| s.mode).unwrap_or_default()
    }

    /// Mode keyed by the internal Haily id — the shape `deliver()` needs (it only has the
    /// Haily id). Unknown session → the safe `Default` (prompt).
    pub fn mode_for_haily(&self, haily_id: &Uuid) -> SessionMode {
        self.acp_id(haily_id).map(|a| self.mode(&a)).unwrap_or_default()
    }

    pub fn set_mode(&self, acp_id: &str, mode: SessionMode) {
        if let Some(mut s) = self.by_acp.get_mut(acp_id) {
            s.mode = mode;
        }
    }

    /// Also settable via the internal id (used when an `allow_always` permission response
    /// switches the session to `AcceptEdits` and only the Haily id is in scope).
    pub fn set_mode_for_haily(&self, haily_id: &Uuid, mode: SessionMode) {
        if let Some(acp) = self.acp_id(haily_id) {
            self.set_mode(&acp, mode);
        }
    }

    /// Record an approval surfaced to the editor, with the cancel token its awaiting
    /// `request_permission` selects on. Firing the token denies that approval immediately.
    pub fn add_pending(&self, haily_id: &Uuid, approval_id: Uuid, cancel: CancellationToken) {
        if let Some(acp) = self.acp_id(haily_id) {
            if let Some(mut s) = self.by_acp.get_mut(&acp) {
                s.pending.insert(approval_id, cancel);
            }
        }
    }

    pub fn remove_pending(&self, haily_id: &Uuid, approval_id: &Uuid) {
        if let Some(acp) = self.acp_id(haily_id) {
            if let Some(mut s) = self.by_acp.get_mut(&acp) {
                s.pending.remove(approval_id);
            }
        }
    }

    /// `session/cancel` fail-safe: fire the cancel token of every pending approval for the
    /// session behind `acp_id`, then clear them. Each awaiting `request_permission` wakes and
    /// resolves its gate as DENIED. Returns how many approvals were cancelled (for logging).
    pub fn cancel_pending(&self, acp_id: &str) -> usize {
        self.by_acp
            .get_mut(acp_id)
            .map(|mut s| {
                let tokens: Vec<CancellationToken> = s.pending.drain().map(|(_, t)| t).collect();
                for t in &tokens {
                    t.cancel();
                }
                tokens.len()
            })
            .unwrap_or(0)
    }

    /// Every live ACP session id (for `session/list`).
    pub fn list(&self) -> Vec<String> {
        self.by_acp.iter().map(|e| e.key().clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_maps_both_directions() {
        let s = AcpSessions::new();
        let (acp, haily) = s.new_session();
        assert_eq!(s.haily_id(&acp), Some(haily));
        assert_eq!(s.acp_id(&haily), Some(acp.clone()));
        assert_eq!(s.mode(&acp), SessionMode::Default, "new session starts in the safe Default mode");
    }

    #[test]
    fn attach_reuses_known_and_mints_for_unknown() {
        let s = AcpSessions::new();
        let (acp, haily) = s.new_session();
        assert_eq!(s.attach(&acp, None), haily, "known acp id reuses its haily id");

        let fresh = s.attach("client-supplied-id", None);
        assert_eq!(s.haily_id("client-supplied-id"), Some(fresh));
    }

    #[test]
    fn mode_is_settable_by_both_keys() {
        let s = AcpSessions::new();
        let (acp, haily) = s.new_session();
        s.set_mode(&acp, SessionMode::DontAsk);
        assert_eq!(s.mode_for_haily(&haily), SessionMode::DontAsk);
        s.set_mode_for_haily(&haily, SessionMode::AcceptEdits);
        assert_eq!(s.mode(&acp), SessionMode::AcceptEdits);
    }

    #[test]
    fn cancel_fires_and_clears_every_pending_approval() {
        let s = AcpSessions::new();
        let (acp, haily) = s.new_session();
        let t1 = CancellationToken::new();
        let t2 = CancellationToken::new();
        s.add_pending(&haily, Uuid::new_v4(), t1.clone());
        s.add_pending(&haily, Uuid::new_v4(), t2.clone());
        assert_eq!(s.cancel_pending(&acp), 2, "cancel must fire every pending approval's token");
        assert!(t1.is_cancelled() && t2.is_cancelled(), "each awaiting request_permission must be woken");
        assert_eq!(s.cancel_pending(&acp), 0, "second cancel is a no-op — pending was cleared");
    }

    #[test]
    fn fork_creates_an_independent_session() {
        let s = AcpSessions::new();
        let (parent, _) = s.new_session();
        let (fork, fork_haily) = s.fork(&parent);
        assert_ne!(parent, fork);
        assert_eq!(s.haily_id(&fork), Some(fork_haily));
    }
}
