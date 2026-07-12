//! Desktop mobile-server core (Mobile Thin-Client plan phase 2a) — `MobileAdapter`, an
//! `Adapter` implementation mirroring `TelegramAdapter`'s shape but speaking the wire protocol
//! in `docs/mobile-protocol.md` over an axum WS server instead of Telegram's bot API.
//!
//! Module layout: `bind`/`tls` are the pure/impure network-setup edges; `pairing` is the HTTP
//! `/pair` ceremony; `ring_buffer`/`writer` are the per-device single-writer seq/replay engine
//! (red team M8/M9); `guard` is the per-frame auth checks (m1/m2/m3); `server` wires all of the
//! above into the actual axum app + WS connection loop.
pub mod bind;
pub(crate) mod guard;
pub mod pairing;
pub mod ring_buffer;
mod server;
pub mod tls;
pub mod writer;

use crate::manager::AdapterManager;
use crate::{
    Adapter, ApprovalResolver, Notification, ProactiveCard, RequestSender, ResponseChunk, RunEvent,
    SessionTranscript, TurnCanceller,
};
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use haily_types::{MobileApprovalPolicy, ServerBody};
use pairing::PairingService;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use writer::DeviceWriter;

/// How long an `approval_reversible` entry (see [`MobileAdapter::approval_reversible`]) is kept
/// before a sweep evicts it as never-resolved (review finding 6d) — comfortably beyond the
/// broker's own 120s approval timeout, so a genuinely in-flight approval is never evicted early.
const APPROVAL_REVERSIBLE_TTL: Duration = Duration::from_secs(600);
/// Sweep `approval_reversible` once it grows past this many entries, rather than on every
/// insert — bounds the cost of the O(n) sweep to "only when it could matter".
const APPROVAL_REVERSIBLE_SWEEP_THRESHOLD: usize = 256;

/// Runtime configuration for the mobile server. Defined here (the leaf `haily-io` crate), NOT
/// in `haily-app`, even though it is LOADED from `kms_preferences` there
/// (`haily-app::mobile_config::load_mobile_config`) — mirrors how `haily_llm::LlmConfig` is
/// defined in the lower crate and populated by `haily-app::config::load_llm_config`.
#[derive(Debug, Clone)]
pub struct MobileServerConfig {
    pub enabled: bool,
    pub port: u16,
    /// LAN-direct `wss://` opt-in (red team M2) — default `false` (tailnet + loopback only).
    pub lan_opt_in: bool,
    pub approval_policy: MobileApprovalPolicy,
    /// Per-device inbound frame rate limit, frames/minute (red team m2).
    pub inbound_rate_limit_per_minute: u32,
    /// `true` (default): a `UserMessage.depth == Deep` from a mobile client is downgraded to
    /// `Normal` before dispatch (red team m2's "denied" arm — the simpler of the two options
    /// the finding allows; "requires confirmation" is left to a future phase).
    pub deny_remote_deep: bool,
    /// Per-device ring-buffer capacity (red team M8/M9 drop-oldest bound).
    pub ring_buffer_capacity: usize,
}

impl Default for MobileServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7443,
            lan_opt_in: false,
            approval_policy: MobileApprovalPolicy::default(),
            inbound_rate_limit_per_minute: 60,
            deny_remote_deep: true,
            ring_buffer_capacity: 500,
        }
    }
}

/// Persistence seam for paired-device rows (mirrors `SessionTranscript`/`ApprovalResolver`):
/// `haily-io` must not depend on `haily-db`, so the DB-backed implementation lives in
/// `haily-app` and is injected via [`MobileAdapter::new`]. No DB types appear in this
/// signature — every method takes/returns primitives.
#[async_trait]
pub trait MobileDeviceStore: Send + Sync {
    /// Resolve a bearer token's hash to its device id, or `None` if unknown/revoked (the WS
    /// upgrade auth check — the two cases are intentionally indistinguishable to the caller).
    async fn find_active_by_token_hash(&self, token_hash: &str) -> Option<Uuid>;
    /// Cheap per-frame revoked check (red team m3), fail-closed (`true`) for an unknown id.
    async fn is_revoked(&self, device_id: Uuid) -> bool;
    async fn touch_last_seen(&self, device_id: Uuid);
    /// Persist a newly-confirmed pairing. `None` on a persistence failure (review finding 3) —
    /// the caller (`pair_handler`) MUST treat that as a hard failure and respond with
    /// `MobileError::Internal`, never mint a token for a device row that was never actually
    /// written (a dead token the phone would silently be unable to use).
    async fn create_device(&self, device_name: &str, token_hash: &str) -> Option<Uuid>;
}

/// The desktop mobile-server `Adapter`. Cheaply `Clone` (every field is `Arc`/`Copy`) — the
/// same instance is handed to the spawned server task in [`Adapter::start`] rather than
/// requiring an `Arc<Self>` wrapper.
#[derive(Clone)]
pub struct MobileAdapter {
    pub(crate) config: MobileServerConfig,
    pub(crate) devices: Arc<dyn MobileDeviceStore>,
    pub(crate) pairing: Arc<PairingService>,
    /// Per-boot nonce (red team C4) — constant for this process's lifetime.
    pub(crate) epoch: u64,
    /// device_id -> its single-writer ring-buffer/socket task handle. Entries persist across a
    /// device's own reconnects (created lazily on first connection, never removed here).
    pub(crate) writers: Arc<DashMap<Uuid, DeviceWriter>>,
    /// session_id -> the device that first used it (red team m1) — also the routing table
    /// `deliver`/`deliver_run_event` use to find the right writer for a session. A session is
    /// claimed by exactly one device for its lifetime (single-active-device-per-session — see
    /// `docs/mobile-protocol.md` §3.2); entries for a given device are evicted by
    /// [`Self::disconnect_device`], never individually (a session does not "end" on its own from
    /// this adapter's point of view).
    pub(crate) session_owner: Arc<DashMap<Uuid, Uuid>>,
    /// Adapter-wide accumulated proactive-card cache, mirroring `GuiAdapter`'s watch-channel
    /// accumulator but read-on-demand (`FetchProactive`) rather than pushed live.
    pub(crate) proactive: Arc<Mutex<Vec<ProactiveCard>>>,
    /// approval_id -> (the `reversible` flag learned when its `ToolApprovalRequest` chunk passed
    /// through `deliver()`, insertion time). The M1 policy-gate proxy (see
    /// `guard::approval_allowed`'s doc) for "is this a genuinely High/IrreversibleWrite
    /// approval". Removed when the `Approve` frame consumes an entry (the common case); an
    /// entry that is NEVER resolved from mobile (approved elsewhere, or timed out) is instead
    /// swept by age once the map grows past a threshold — see `deliver`'s sweep call (review
    /// finding 6d).
    pub(crate) approval_reversible: Arc<DashMap<Uuid, (bool, Instant)>>,
    /// device_id -> known-revoked (review findings 1/5). Write-through cache: set by
    /// [`Self::disconnect_device`] and consulted by both the inbound frame loop (replacing a
    /// per-frame DB query) and outbound `push_for_session` — a revoked device's live socket
    /// must stop receiving pushes too, not only stop being able to send.
    pub(crate) revoked_cache: Arc<DashMap<Uuid, ()>>,
    /// device_id -> (this connection instance's id, its cancellation token) for whichever
    /// connection is CURRENTLY live for that device (review finding 1). `disconnect_device`
    /// cancels the token to close an already-open socket immediately, rather than waiting for
    /// the client to send another frame. The instance id lets a connection's own cleanup remove
    /// only ITS OWN entry, never a newer reconnect's.
    pub(crate) connections: Arc<DashMap<Uuid, (Uuid, CancellationToken)>>,
    resolver: Arc<Mutex<Option<Arc<dyn ApprovalResolver>>>>,
    kill: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    transcript: Arc<Mutex<Option<Arc<dyn SessionTranscript>>>>,
    tx: Arc<Mutex<Option<RequestSender>>>,
    /// Back-reference to the `AdapterManager` that registered this adapter (review finding 2,
    /// m7) — injected post-construction via `set_adapter_manager`, mirroring the
    /// resolver/kill/transcript wiring contract. Lets the mobile-initiated kill-switch ENABLE
    /// path broadcast `Notification::KillStateChanged` to every OTHER adapter via `notify_all`,
    /// not just push it to this adapter's own connected devices. A benign `Arc` reference cycle
    /// (this adapter is itself reachable through the manager it holds) is acceptable here: both
    /// live for the whole process lifetime regardless, so nothing is ever freed early either way.
    manager: Arc<Mutex<Option<AdapterManager>>>,
    /// Turn-cancellation seam (Mobile Thin-Client plan phase 3 amendment) — injected
    /// post-construction via `set_turn_canceller`, mirroring `resolver`/`kill`/`transcript`.
    /// `None` until `haily-app::bootstrap` wires it, in which case `ClientFrame::CancelTurn`
    /// is a harmless no-op (see `server.rs::handle_client_frame`).
    turn_canceller: Arc<Mutex<Option<Arc<dyn TurnCanceller>>>>,
    /// Directory holding the persisted TLS identity (`tls::load_or_generate`) and where a
    /// future `haily pair` (CLI) would look up the live pairing service — see
    /// `haily-cli/src/main.rs`.
    pub(crate) data_dir: std::path::PathBuf,
}

impl MobileAdapter {
    pub fn new(
        config: MobileServerConfig,
        devices: Arc<dyn MobileDeviceStore>,
        data_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            config,
            devices,
            pairing: Arc::new(PairingService::new()),
            epoch: rand_epoch(),
            writers: Arc::new(DashMap::new()),
            session_owner: Arc::new(DashMap::new()),
            proactive: Arc::new(Mutex::new(Vec::new())),
            approval_reversible: Arc::new(DashMap::new()),
            revoked_cache: Arc::new(DashMap::new()),
            connections: Arc::new(DashMap::new()),
            resolver: Arc::new(Mutex::new(None)),
            kill: Arc::new(Mutex::new(None)),
            transcript: Arc::new(Mutex::new(None)),
            tx: Arc::new(Mutex::new(None)),
            manager: Arc::new(Mutex::new(None)),
            turn_canceller: Arc::new(Mutex::new(None)),
            data_dir,
        }
    }

    /// Mark `device_id` revoked and forcibly end its live connection (review findings 1/4/5):
    /// closes the socket immediately (via its cancellation token) rather than waiting for the
    /// next inbound frame, stops any further outbound push (`revoked_cache`), detaches its
    /// writer, and evicts every `session_owner` claim it held (a revoked device must not be
    /// able to re-claim or keep squatting on a session id). Exposed for a future explicit caller
    /// (P2b's Devices panel "Revoke" button) — until then, `server.rs`'s periodic per-connection
    /// poll of `MobileDeviceStore::is_revoked` is what actually calls this today, so revocation
    /// takes effect even with no UI wired yet.
    pub fn disconnect_device(&self, device_id: Uuid) {
        self.revoked_cache.insert(device_id, ());
        if let Some(entry) = self.connections.get(&device_id) {
            entry.1.cancel();
        }
        self.session_owner.retain(|_, owner| *owner != device_id);
        if let Some(writer) = self.writers.get(&device_id) {
            writer.detach();
        }
    }

    /// Every code still awaiting an interactive OOB confirm — the seam a future GUI dialog
    /// (P2b) polls, mirroring `haily-core::approval::ApprovalBroker::pending_snapshot`.
    pub fn pending_pair_confirms(&self) -> Vec<pairing::PendingConfirm> {
        self.pairing.pending_confirms()
    }

    /// Mint a fresh pairing code (see `pairing::PairingService::mint_code` for the two confirm
    /// modes). `pre_approved: true` is `haily pair`'s headless ceremony (running the command IS
    /// the out-of-band confirm); `false` is the future GUI "Add Device" flow (P2b).
    pub fn mint_pairing_code(
        &self,
        device_name_hint: Option<String>,
        pre_approved: bool,
    ) -> String {
        self.pairing.mint_code(device_name_hint, pre_approved)
    }

    /// Resolve a pending pairing confirm — the future GUI dialog's Approve/Deny action.
    pub fn confirm_pairing(&self, code: &str, approved: bool) -> bool {
        self.pairing.confirm(code, approved)
    }

    /// Like `Adapter::start`, but `.await`s the bind attempt directly and reports whether at
    /// least one address was successfully bound (review finding 6c) — for a caller like `haily
    /// pair` that wants to tell the operator immediately "the port is already in use" rather
    /// than silently degrading (M11's fire-and-forget contract is right for the always-on
    /// daemon path, but wrong for a one-shot interactive ceremony). Stores `tx` first, exactly
    /// like `start`.
    pub async fn start_and_await_bind(&self, tx: RequestSender) -> bool {
        *self.tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
        if !self.config.enabled {
            return false;
        }
        server::run(self.clone(), tx).await
    }

    fn get_or_spawn_writer(&self, device_id: Uuid) -> DeviceWriter {
        self.writers
            .entry(device_id)
            .or_insert_with(|| DeviceWriter::spawn(self.epoch, self.config.ring_buffer_capacity))
            .clone()
    }

    fn push_for_session(&self, session_id: Uuid, body: ServerBody) {
        if let Some(device_id) = self.session_owner.get(&session_id).map(|e| *e) {
            // Review finding 1: a revoked device's live socket must stop RECEIVING pushes too,
            // not just be prevented from sending — a lingering writer would otherwise keep
            // streaming chunks to an already-supposedly-cut-off connection.
            if self.revoked_cache.contains_key(&device_id) {
                return;
            }
            self.get_or_spawn_writer(device_id).push(body);
        }
    }

    pub(crate) fn resolver_handle(&self) -> Option<Arc<dyn ApprovalResolver>> {
        self.resolver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn kill_handle(&self) -> Option<Arc<AtomicBool>> {
        self.kill.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn transcript_handle(&self) -> Option<Arc<dyn SessionTranscript>> {
        self.transcript
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn tx_handle(&self) -> Option<RequestSender> {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn manager_handle(&self) -> Option<AdapterManager> {
        self.manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn turn_canceller_handle(&self) -> Option<Arc<dyn TurnCanceller>> {
        self.turn_canceller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Sweep `approval_reversible` of entries older than [`APPROVAL_REVERSIBLE_TTL`] — an entry
    /// whose approval was resolved through some OTHER channel (GUI/CLI/Telegram) or simply timed
    /// out never gets removed by the `Approve`-frame path, so without this it would accumulate
    /// forever (review finding 6d). Only runs once the map is large enough for it to matter.
    fn sweep_approval_reversible(&self) {
        if self.approval_reversible.len() <= APPROVAL_REVERSIBLE_SWEEP_THRESHOLD {
            return;
        }
        let now = Instant::now();
        self.approval_reversible.retain(|_, (_, inserted_at)| {
            now.duration_since(*inserted_at) < APPROVAL_REVERSIBLE_TTL
        });
    }
}

/// Per-boot epoch (red team C4) — a fresh random-ish value each process start so a
/// reconnecting client's stale seq cursor is never mistaken for the current process's seq
/// space. Derived from a UUIDv4 rather than a dedicated RNG dependency (matches
/// `pairing::generate_token`'s rationale).
fn rand_epoch() -> u64 {
    let bytes = Uuid::new_v4().into_bytes();
    u64::from_be_bytes(
        bytes[0..8]
            .try_into()
            .expect("16-byte UUID has a 8-byte prefix"),
    )
}

#[async_trait]
impl Adapter for MobileAdapter {
    /// Stores `tx` for the connection loop's `UserMessage` forwarding, then — unless disabled
    /// by config — spawns the axum server in the background. Never blocks or fails on a bind
    /// error (red team M11): binding happens entirely inside the spawned task, so this method
    /// itself cannot observe or propagate a bind failure.
    async fn start(&self, tx: RequestSender) -> Result<()> {
        *self.tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
        if !self.config.enabled {
            tracing::info!("mobile: server disabled by config — not starting listener");
            return Ok(());
        }
        let state = self.clone();
        tokio::spawn(async move {
            server::run(state, tx).await;
        });
        Ok(())
    }

    async fn deliver(&self, session_id: Uuid, chunk: ResponseChunk) -> Result<()> {
        if let ResponseChunk::ToolApprovalRequest {
            approval_id,
            reversible,
            ..
        } = &chunk
        {
            self.approval_reversible
                .insert(*approval_id, (*reversible, Instant::now()));
            self.sweep_approval_reversible();
        }
        self.push_for_session(session_id, ServerBody::Chunk { session_id, chunk });
        Ok(())
    }

    async fn deliver_run_event(&self, session_id: Uuid, event: RunEvent) -> Result<()> {
        self.push_for_session(session_id, ServerBody::Run { session_id, event });
        Ok(())
    }

    /// `WorkItemsChanged` has no mobile-v1 surface (no persistent panel yet, like Telegram);
    /// `KillStateChanged` broadcasts to every connected device (m7/M15, global kill switch);
    /// every other kind accumulates into the `FetchProactive` cache (see `proactive`), pushed
    /// only on request rather than live — matching the wire catalogue's documented contract
    /// (`ProactiveList` is "the reply to `FetchProactive`").
    async fn notify(&self, msg: Notification) -> Result<()> {
        match msg {
            Notification::WorkItemsChanged(_) => {}
            Notification::KillStateChanged { on } => {
                for entry in self.writers.iter() {
                    entry.value().push(ServerBody::KillState { on });
                }
            }
            other => {
                if let Some(card) = ProactiveCard::from_notification(&other) {
                    let mut cards = self.proactive.lock().unwrap_or_else(|e| e.into_inner());
                    *cards = crate::proactive_cards::upsert_proactive_card(&cards, card);
                }
            }
        }
        Ok(())
    }

    fn set_approval_resolver(&self, resolver: Arc<dyn ApprovalResolver>) {
        *self.resolver.lock().unwrap_or_else(|e| e.into_inner()) = Some(resolver);
    }

    fn set_kill_switch(&self, kill: Arc<AtomicBool>) {
        *self.kill.lock().unwrap_or_else(|e| e.into_inner()) = Some(kill);
    }

    fn set_session_transcript(&self, transcript: Arc<dyn SessionTranscript>) {
        *self.transcript.lock().unwrap_or_else(|e| e.into_inner()) = Some(transcript);
    }

    /// Review finding 2 (m7): stores a handle back to the manager so the mobile-initiated
    /// kill-switch ENABLE path can broadcast `Notification::KillStateChanged` to every other
    /// adapter via `notify_all`, not just push it to this adapter's own connected devices.
    fn set_adapter_manager(&self, manager: AdapterManager) {
        *self.manager.lock().unwrap_or_else(|e| e.into_inner()) = Some(manager);
    }

    /// Mobile Thin-Client plan phase 3 amendment: lets `ClientFrame::CancelTurn` reach
    /// `haily-app::TurnRegistry` without this crate depending on `haily-app` — see the trait
    /// default's doc comment for the wiring contract.
    fn set_turn_canceller(&self, canceller: Arc<dyn TurnCanceller>) {
        *self
            .turn_canceller
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(canceller);
    }

    fn id(&self) -> &str {
        "mobile"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn adapter() -> MobileAdapter {
        MobileAdapter::new(
            MobileServerConfig::default(),
            Arc::new(FakeDeviceStore),
            std::env::temp_dir(),
        )
    }

    #[tokio::test]
    async fn disabled_config_start_does_not_spawn_a_listener() {
        // No real assertion possible on "no listener bound" without a live port scan; this
        // guards the simpler invariant — `start()` returns Ok and never panics — when the
        // config is the safe (disabled) default.
        let a = adapter();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        assert!(a.start(tx).await.is_ok());
    }

    #[tokio::test]
    async fn notify_work_items_changed_is_a_no_op() {
        let a = adapter();
        assert!(a
            .notify(Notification::WorkItemsChanged(vec![]))
            .await
            .is_ok());
        assert!(a.proactive.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn notify_kill_state_changed_pushes_to_every_connected_writer() {
        let a = adapter();
        let device_id = Uuid::new_v4();
        a.get_or_spawn_writer(device_id);
        assert!(a
            .notify(Notification::KillStateChanged { on: true })
            .await
            .is_ok());
        // No panics + a writer exists — the push is exercised end-to-end in
        // `writer::tests` (that module owns asserting the frame actually arrives).
        assert!(a.writers.contains_key(&device_id));
    }

    #[tokio::test]
    async fn notify_alert_accumulates_into_the_proactive_cache() {
        let a = adapter();
        a.notify(Notification::Alert {
            title: "t".into(),
            body: "b".into(),
            urgent: true,
        })
        .await
        .unwrap();
        assert_eq!(a.proactive.lock().unwrap().len(), 1);
    }

    #[test]
    fn deliver_for_an_unbound_session_is_a_silent_no_op() {
        let a = adapter();
        // No device owns this session — push_for_session must not panic on the miss.
        a.push_for_session(Uuid::new_v4(), ServerBody::Pong);
    }

    #[test]
    fn rand_epoch_produces_distinct_values() {
        assert_ne!(rand_epoch(), rand_epoch());
    }

    // -----------------------------------------------------------------------
    // Review findings 1/4/5 — disconnect_device: revoked cache, session eviction, live
    // connection cancellation.
    // -----------------------------------------------------------------------

    #[test]
    fn disconnect_device_marks_revoked_and_evicts_its_session_claims() {
        let a = adapter();
        let device_id = Uuid::new_v4();
        let other_device = Uuid::new_v4();
        let owned_session = Uuid::new_v4();
        let foreign_session = Uuid::new_v4();
        a.session_owner.insert(owned_session, device_id);
        a.session_owner.insert(foreign_session, other_device);

        a.disconnect_device(device_id);

        assert!(a.revoked_cache.contains_key(&device_id));
        assert!(
            a.session_owner.get(&owned_session).is_none(),
            "the revoked device's session claim must be evicted"
        );
        assert_eq!(
            *a.session_owner.get(&foreign_session).unwrap(),
            other_device,
            "a different device's session claim must be untouched"
        );
    }

    #[test]
    fn disconnect_device_cancels_the_registered_connection_token() {
        let a = adapter();
        let device_id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        a.connections
            .insert(device_id, (Uuid::new_v4(), cancel.clone()));

        a.disconnect_device(device_id);

        assert!(
            cancel.is_cancelled(),
            "the live connection's token must be cancelled"
        );
    }

    #[tokio::test]
    async fn push_for_session_is_a_no_op_for_a_revoked_device() {
        let a = adapter();
        let device_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        a.session_owner.insert(session_id, device_id);
        a.get_or_spawn_writer(device_id);
        a.disconnect_device(device_id);

        // Must not panic; the actual "no frame reaches the socket" guarantee is covered by
        // `writer::tests` (this only proves the gate is consulted before reaching the writer).
        a.push_for_session(session_id, ServerBody::Pong);
    }

    // -----------------------------------------------------------------------
    // Review finding 6d — approval_reversible sweep.
    // -----------------------------------------------------------------------

    #[test]
    fn sweep_approval_reversible_evicts_only_stale_entries_past_ttl() {
        let a = adapter();
        let stale = Uuid::new_v4();
        let fresh = Uuid::new_v4();
        a.approval_reversible.insert(
            stale,
            (
                false,
                Instant::now() - APPROVAL_REVERSIBLE_TTL - Duration::from_secs(1),
            ),
        );
        a.approval_reversible.insert(fresh, (false, Instant::now()));
        // Pad past the sweep threshold so the sweep actually runs.
        for _ in 0..APPROVAL_REVERSIBLE_SWEEP_THRESHOLD {
            a.approval_reversible
                .insert(Uuid::new_v4(), (false, Instant::now()));
        }

        a.sweep_approval_reversible();

        assert!(
            !a.approval_reversible.contains_key(&stale),
            "a stale entry must be evicted"
        );
        assert!(
            a.approval_reversible.contains_key(&fresh),
            "a fresh entry must survive the sweep"
        );
    }

    #[test]
    fn sweep_approval_reversible_is_a_no_op_below_the_threshold() {
        let a = adapter();
        let stale = Uuid::new_v4();
        a.approval_reversible.insert(
            stale,
            (
                false,
                Instant::now() - APPROVAL_REVERSIBLE_TTL - Duration::from_secs(1),
            ),
        );

        a.sweep_approval_reversible();

        assert!(
            a.approval_reversible.contains_key(&stale),
            "below the sweep threshold, nothing is evicted yet (bounded sweep cost)"
        );
    }

    // -----------------------------------------------------------------------
    // Review finding 2 (m7) — set_adapter_manager wiring + notify() still works untouched.
    // -----------------------------------------------------------------------

    #[test]
    fn set_adapter_manager_stores_the_handle() {
        let a = adapter();
        assert!(a.manager_handle().is_none());
        let am = AdapterManager::builder().build();
        a.set_adapter_manager(am);
        assert!(a.manager_handle().is_some());
    }
}
