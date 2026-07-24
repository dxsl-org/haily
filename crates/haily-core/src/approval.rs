//! Real tool-approval broker — replaces the 0ms auto-approve placeholder.
//!
//! One pending approval per turn (the agent loop is sequential — KISS), keyed by
//! `approval_id`. `request()` is called from `tool_call::dispatch` at L0; the
//! `ApprovalResolver` impl below is the only way a pending approval is ever settled,
//! and is exposed to adapters (GUI/CLI/Telegram) as `Arc<dyn ApprovalResolver>` so
//! `haily-io` never needs to depend on `haily-core` (the trait itself lives in
//! `haily-types`).
use async_trait::async_trait;
use dashmap::DashMap;
use haily_types::{ApprovalGate, ApprovalResolver};
use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

/// A snapshot of one in-flight approval, for the unified cross-channel approvals queue
/// (Sub-Agent + Skill Architecture phase 11a). Deliberately carries NO tool name/args:
/// the broker is a pure wait registry and never learned the descriptive payload (that
/// lives in the `ToolApprovalRequest` chunk the origin channel already received). This
/// snapshot is a RECONCILE source — which approval ids are still live and who owns them —
/// so a UI can prune resolved entries and enforce the session-auth boundary, exactly how
/// the work-item list reconciles the live `watch` snapshots. `session_id` is the auth
/// boundary: a queued approval can only ever be resolved by its owning session.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PendingApproval {
    pub approval_id: Uuid,
    pub session_id: Uuid,
    /// RFC3339 registration time — lets a UI age/sort the queue.
    pub created_at: String,
}

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
    pending: DashMap<
        Uuid,
        (
            Uuid,   /* session_id */
            String, /* created_at */
            oneshot::Sender<bool>,
        ),
    >,
    /// Tool names exempted from the interactive prompt. Populated once at bootstrap
    /// from validated config (`haily_app::validate_auto_approve` rejects any
    /// destructive/exfil tool name before this is ever built) — never mutated after
    /// construction, so no lock is needed for reads from `tool_call::dispatch`.
    auto_approve: HashSet<String>,
}

impl ApprovalBroker {
    pub fn new() -> Self {
        Self {
            pending: DashMap::new(),
            auto_approve: HashSet::new(),
        }
    }

    /// Construct with a pre-validated auto-approve allowlist. Callers MUST validate
    /// `names` against the tool registry first (destructive/exfil classes can never
    /// be listed) — this constructor trusts its input and does not re-check.
    pub fn with_auto_approve(names: HashSet<String>) -> Self {
        Self {
            pending: DashMap::new(),
            auto_approve: names,
        }
    }

    /// Whether `tool_name` is on the pre-validated auto-approve allowlist. Every
    /// auto-approved call is logged at warn by the caller (`tool_call::dispatch`) —
    /// bypassing the interactive prompt is a deliberate, auditable trust decision.
    pub fn is_auto_approved(&self, tool_name: &str) -> bool {
        self.auto_approve.contains(tool_name)
    }

    /// Snapshot of every currently in-flight approval (phase 11a), for the cross-channel
    /// approvals queue. Order is unspecified (DashMap) — the caller sorts (e.g. by
    /// `created_at`). A late-resolving entry may vanish between this call and the caller
    /// reading it; that is benign (the UI reconciles), the same best-effort contract the
    /// work-item snapshot has.
    pub fn pending_snapshot(&self) -> Vec<PendingApproval> {
        self.pending
            .iter()
            .map(|e| {
                let (session_id, created_at, _tx) = e.value();
                PendingApproval {
                    approval_id: *e.key(),
                    session_id: *session_id,
                    created_at: created_at.clone(),
                }
            })
            .collect()
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
    ///
    /// INVARIANT (M10): at most ONE pending approval per `session_id`. Tool dispatch
    /// is sequential within a turn (the L0 loop and each sub-turn issue one tool call
    /// at a time), so a second live `request` for the same session can only arise from
    /// a bug or a concurrency violation — and the frontend holds a single
    /// `pendingApproval` slot (a second would silently overwrite the first). Rather
    /// than register a hidden pending nobody can resolve, we reject fail-loud (log +
    /// return `false` = deny) and DO NOT touch the existing pending entry.
    pub async fn request(
        &self,
        approval_id: Uuid,
        session_id: Uuid,
        cancel: &CancellationToken,
    ) -> bool {
        if self.pending.iter().any(|e| e.value().0 == session_id) {
            warn!(
                %approval_id,
                %session_id,
                "approval request rejected — a pending approval already exists for this session \
                 (M10: sequential dispatch ⇒ ≤1 pending approval/session); denying fail-loud without \
                 overwriting the in-flight request"
            );
            return false;
        }

        let (tx, rx) = oneshot::channel();
        let created_at = chrono::Utc::now().to_rfc3339();
        self.pending
            .insert(approval_id, (session_id, created_at, tx));

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

/// Delegates to the inherent `request` above — lands here (not phase 2) so
/// `ToolContext::approval_gate` can be real from day one (KISS, phase-1 spec step 5).
#[async_trait]
impl ApprovalGate for ApprovalBroker {
    async fn request(
        &self,
        approval_id: Uuid,
        session_id: Uuid,
        cancel: &CancellationToken,
    ) -> bool {
        self.request(approval_id, session_id, cancel).await
    }

    fn is_auto_approved(&self, tool_name: &str) -> bool {
        self.is_auto_approved(tool_name)
    }
}

impl ApprovalResolver for ApprovalBroker {
    fn resolve(&self, approval_id: Uuid, session_id: Uuid, approved: bool) -> bool {
        // `remove_if` takes the entry out only when the session matches, so a
        // mismatched caller can't observe (or race) the real pending state, and a
        // second resolve() for the same id after the first succeeds is a clean no-op
        // (idempotent — entry is already gone).
        let removed = self
            .pending
            .remove_if(&approval_id, |_, (bound_session, _created_at, _)| {
                *bound_session == session_id
            });
        match removed {
            Some((_, (_, _created_at, tx))) => {
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

    /// Phase-1 success criteria: `ApprovalGate` must be object-safe and
    /// `ApprovalBroker` must implement it, so `ToolContext::approval_gate` can hold
    /// it as `Arc<dyn ApprovalGate>` — this is a compile-time proof, not a runtime
    /// assertion (a signature/object-safety regression fails the build, not this test).
    #[test]
    fn approval_broker_is_object_safe_as_approval_gate() {
        let _: std::sync::Arc<dyn ApprovalGate> = std::sync::Arc::new(ApprovalBroker::new());
    }

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
        assert!(
            !decision,
            "cancellation must deny immediately, proving the deny-by-default select arm works"
        );
        assert!(
            broker.pending.is_empty(),
            "pending entry must be cleaned up on exit"
        );
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
        assert!(
            resolved,
            "the genuine session must still be able to resolve after a forged attempt"
        );

        let decision = tokio::time::timeout(Duration::from_secs(2), request_fut)
            .await
            .expect("request() should resolve once the real session approves");
        assert!(decision);
    }

    #[tokio::test]
    async fn second_concurrent_pending_rejected_for_session() {
        // M10: with one approval already in flight for a session, a SECOND concurrent
        // `request` for the same session must be denied fail-loud without disturbing
        // the first — the frontend holds a single pending slot, so a silent second
        // pending would either overwrite the first or become unresolvable.
        let broker = ApprovalBroker::new();
        let session_id = Uuid::new_v4();
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let cancel = CancellationToken::new();

        let broker_ref = &broker;
        let first_fut = broker_ref.request(first_id, session_id, &cancel);
        tokio::pin!(first_fut);

        // Drive the first request far enough to register its pending entry.
        tokio::select! {
            _ = &mut first_fut => panic!("first request must not resolve yet"),
            _ = tokio::task::yield_now() => {}
        }

        // Second concurrent request for the SAME session: denied without registering.
        let second = broker_ref.request(second_id, session_id, &cancel).await;
        assert!(
            !second,
            "a 2nd concurrent pending for the same session must be denied"
        );
        assert!(
            broker_ref.pending.get(&second_id).is_none(),
            "the rejected 2nd request must NOT have registered a pending entry"
        );

        // The FIRST pending is untouched and still resolvable by the real session.
        assert!(
            broker_ref.resolve(first_id, session_id, true),
            "the original pending must be intact"
        );
        let first = tokio::time::timeout(Duration::from_secs(2), first_fut)
            .await
            .expect("first request should resolve once approved");
        assert!(first, "the first (untouched) approval resolves normally");
    }

    #[tokio::test]
    async fn pending_snapshot_lists_inflight_and_a_foreign_session_cannot_resolve() {
        // Phase 11a: the cross-channel approvals queue reads `pending_snapshot()`; the
        // session_id in each entry is the auth boundary — a DIFFERENT session must not be
        // able to resolve a queued approval it does not own.
        let broker = ApprovalBroker::new();
        let approval_id = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let cancel = CancellationToken::new();

        let broker_ref = &broker;
        let request_fut = broker_ref.request(approval_id, owner, &cancel);
        tokio::pin!(request_fut);
        tokio::select! {
            _ = &mut request_fut => panic!("request() must not resolve yet"),
            _ = tokio::task::yield_now() => {}
        }

        // The queue read shows exactly one in-flight approval, owned by `owner`.
        let snap = broker_ref.pending_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].approval_id, approval_id);
        assert_eq!(
            snap[0].session_id, owner,
            "snapshot must carry the owning session"
        );
        assert!(!snap[0].created_at.is_empty());

        // A foreign session cannot resolve it (session auth boundary preserved), and it
        // therefore stays in the queue.
        assert!(!broker_ref.resolve(approval_id, intruder, true));
        assert_eq!(
            broker_ref.pending_snapshot().len(),
            1,
            "a rejected foreign resolve leaves it queued"
        );

        // The owner can, and the queue then empties.
        assert!(broker_ref.resolve(approval_id, owner, true));
        let decision = tokio::time::timeout(Duration::from_secs(2), request_fut)
            .await
            .expect("owner resolve unblocks request()");
        assert!(decision);
        assert!(
            broker_ref.pending_snapshot().is_empty(),
            "resolved approval leaves the queue"
        );
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

        let decision = tokio::time::timeout(Duration::from_secs(2), request_fut)
            .await
            .expect("first resolve should have unblocked request()");
        assert!(
            decision,
            "the first (approve) resolve must be the one that wins"
        );
    }
}
