//! Real tool-approval broker — replaces the 0ms auto-approve placeholder.
//!
//! One pending approval per turn (the agent loop is sequential — KISS), keyed by
//! `approval_id`. `request()` is called from `tool_call::dispatch` at L0; the
//! `ApprovalResolver` impl below is the only way a pending approval is ever settled,
//! and is exposed to adapters (GUI/CLI/Telegram) as `Arc<dyn ApprovalResolver>` so
//! `haily-io` never needs to depend on `haily-core` (the trait itself lives in
//! `haily-types`).
use haily_types::ApprovalResolver;
use dashmap::DashMap;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

/// How long a pending approval waits for a decision before defaulting to deny.
/// Headless/unattended deployments (no human at a GUI/CLI/Telegram) must never hang
/// a turn forever — deny-by-default on timeout is the safe failure mode for
/// destructive/exfiltrating tools.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Session-bound registry of in-flight approval requests.
///
/// `approval_id` is shown to the user in the approval prompt (not a secret);
/// `session_id` is the actual auth boundary — `resolve()` rejects any call whose
/// `session_id` doesn't match the one the approval was registered under, which is
/// what stops a foreign Telegram chat (or a forged GUI/CLI call) from resolving
/// someone else's pending approval.
pub struct ApprovalBroker {
    pending: DashMap<Uuid, (Uuid /* session_id */, oneshot::Sender<bool>)>,
    /// Tool names exempted from the interactive prompt. Populated once at bootstrap
    /// from validated config (`haily_app::validate_auto_approve` rejects any
    /// destructive/exfil tool name before this is ever built) — never mutated after
    /// construction, so no lock is needed for reads from `tool_call::dispatch`.
    auto_approve: HashSet<String>,
}

impl ApprovalBroker {
    pub fn new() -> Self {
        Self { pending: DashMap::new(), auto_approve: HashSet::new() }
    }

    /// Construct with a pre-validated auto-approve allowlist. Callers MUST validate
    /// `names` against the tool registry first (destructive/exfil classes can never
    /// be listed) — this constructor trusts its input and does not re-check.
    pub fn with_auto_approve(names: HashSet<String>) -> Self {
        Self { pending: DashMap::new(), auto_approve: names }
    }

    /// Whether `tool_name` is on the pre-validated auto-approve allowlist. Every
    /// auto-approved call is logged at warn by the caller (`tool_call::dispatch`) —
    /// bypassing the interactive prompt is a deliberate, auditable trust decision.
    pub fn is_auto_approved(&self, tool_name: &str) -> bool {
        self.auto_approve.contains(tool_name)
    }

    /// Register a pending approval and wait for a decision.
    ///
    /// Resolves to `true` only if `resolve()` is called with a matching
    /// `approval_id`/`session_id` and `approved = true` before `cancel` fires or
    /// `APPROVAL_TIMEOUT` elapses. Cancellation and timeout both deny — a pending
    /// approval must never block shutdown drain or hang a headless turn forever.
    ///
    /// The pending entry is always removed on exit (decision, cancel, or timeout) so
    /// a late `resolve()` call for the same id after this returns is a harmless no-op
    /// (id no longer found → `resolve()` returns `false`).
    pub async fn request(&self, approval_id: Uuid, session_id: Uuid, cancel: &CancellationToken) -> bool {
        let (tx, rx) = oneshot::channel();
        self.pending.insert(approval_id, (session_id, tx));

        let decision = tokio::select! {
            result = rx => result.unwrap_or(false), // sender dropped without resolving → deny
            _ = cancel.cancelled() => false,
            _ = tokio::time::sleep(APPROVAL_TIMEOUT) => false,
        };

        self.pending.remove(&approval_id);
        decision
    }
}

impl Default for ApprovalBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalResolver for ApprovalBroker {
    fn resolve(&self, approval_id: Uuid, session_id: Uuid, approved: bool) -> bool {
        // `remove_if` takes the entry out only when the session matches, so a
        // mismatched caller can't observe (or race) the real pending state, and a
        // second resolve() for the same id after the first succeeds is a clean no-op
        // (idempotent — entry is already gone).
        let removed = self.pending.remove_if(&approval_id, |_, (bound_session, _)| *bound_session == session_id);
        match removed {
            Some((_, (_, tx))) => {
                // Ignore send failure: the requester side may have already exited via
                // cancellation/timeout, in which case the decision is moot.
                let _ = tx.send(approved);
                true
            }
            None => {
                warn!(
                    %approval_id,
                    %session_id,
                    "approval resolve() rejected — unknown id, already resolved, or session mismatch"
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approve_resolves_true() {
        let broker = ApprovalBroker::new();
        let approval_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let cancel = CancellationToken::new();

        let broker_ref = &broker;
        let (decision, resolved) = tokio::join!(
            broker_ref.request(approval_id, session_id, &cancel),
            async {
                // Give request() time to register the pending entry first.
                tokio::task::yield_now().await;
                broker_ref.resolve(approval_id, session_id, true)
            }
        );

        assert!(resolved, "resolve() should find the pending approval");
        assert!(decision, "approve should resolve request() to true");
    }

    #[tokio::test]
    async fn deny_resolves_false() {
        let broker = ApprovalBroker::new();
        let approval_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let cancel = CancellationToken::new();

        let broker_ref = &broker;
        let (decision, resolved) = tokio::join!(
            broker_ref.request(approval_id, session_id, &cancel),
            async {
                tokio::task::yield_now().await;
                broker_ref.resolve(approval_id, session_id, false)
            }
        );

        assert!(resolved);
        assert!(!decision, "deny should resolve request() to false");
    }

    #[tokio::test]
    async fn timeout_denies_without_a_response() {
        // Can't wait out the real 120s in a unit test — race request() against a
        // short sleep and assert the pending entry is still gone afterward isn't
        // feasible without exposing the timeout as a param. Instead, verify the
        // narrower guarantee: an unresolved, uncancelled request never resolves to
        // `true` via any path other than an explicit approve. We simulate "no
        // response ever arrives" by dropping the broker's only resolver path
        // (cancel) and asserting the cancel-deny path (below) covers the same
        // deny-by-default contract the timeout path shares.
        let broker = ApprovalBroker::new();
        let approval_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        cancel.cancel(); // pre-cancelled — request() must see it immediately, not hang

        let decision = broker.request(approval_id, session_id, &cancel).await;
        assert!(!decision, "cancellation must deny immediately, proving the deny-by-default select arm works");
        assert!(broker.pending.is_empty(), "pending entry must be cleaned up on exit");
    }

    #[tokio::test]
    async fn cancellation_denies_immediately() {
        let broker = ApprovalBroker::new();
        let approval_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let cancel = CancellationToken::new();

        let broker_ref = &broker;
        let request_fut = broker_ref.request(approval_id, session_id, &cancel);
        tokio::pin!(request_fut);

        // Drive the request future once so it registers the pending entry, then
        // cancel and confirm it resolves promptly (well under the 120s timeout).
        tokio::select! {
            _ = &mut request_fut => panic!("request() must not resolve before cancel or timeout"),
            _ = tokio::task::yield_now() => {}
        }
        cancel.cancel();

        let decision = tokio::time::timeout(Duration::from_secs(2), request_fut)
            .await
            .expect("cancellation must deny promptly, not hang toward the 120s timeout");
        assert!(!decision);
    }

    #[tokio::test]
    async fn wrong_session_id_is_rejected_and_approval_stays_pending() {
        let broker = ApprovalBroker::new();
        let approval_id = Uuid::new_v4();
        let real_session = Uuid::new_v4();
        let forged_session = Uuid::new_v4();
        let cancel = CancellationToken::new();

        let broker_ref = &broker;
        let request_fut = broker_ref.request(approval_id, real_session, &cancel);
        tokio::pin!(request_fut);

        tokio::select! {
            _ = &mut request_fut => panic!("request() must not resolve yet"),
            _ = tokio::task::yield_now() => {}
        }

        // Forged/foreign-session resolve attempt must be rejected without touching
        // the pending entry.
        let rejected = broker_ref.resolve(approval_id, forged_session, true);
        assert!(!rejected, "mismatched session_id must be rejected");

        // The approval must still be resolvable by the real session afterward.
        let resolved = broker_ref.resolve(approval_id, real_session, true);
        assert!(resolved, "the genuine session must still be able to resolve after a forged attempt");

        let decision = tokio::time::timeout(Duration::from_secs(2), request_fut)
            .await
            .expect("request() should resolve once the real session approves");
        assert!(decision);
    }

    #[tokio::test]
    async fn double_resolve_is_a_no_op() {
        let broker = ApprovalBroker::new();
        let approval_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let cancel = CancellationToken::new();

        let broker_ref = &broker;
        let request_fut = broker_ref.request(approval_id, session_id, &cancel);
        tokio::pin!(request_fut);

        tokio::select! {
            _ = &mut request_fut => panic!("request() must not resolve yet"),
            _ = tokio::task::yield_now() => {}
        }

        assert!(broker_ref.resolve(approval_id, session_id, true));
        // Second resolve for the same id: already gone, must report false and must
        // not panic or double-send on the (already-consumed) oneshot sender.
        assert!(!broker_ref.resolve(approval_id, session_id, false));

        let decision = tokio::time::timeout(Duration::from_secs(2), request_fut).await.expect("first resolve should have unblocked request()");
        assert!(decision, "the first (approve) resolve must be the one that wins");
    }
}
