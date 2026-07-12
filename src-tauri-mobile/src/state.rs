//! Command-facing app state: the live WS client handle (if paired+connected), the current
//! connection/pairing status snapshot, and pending `FetchSession` request/response correlation.
use dashmap::DashMap;
use haily_mobile_client::ClientHandle;
use haily_types::SessionSnapshot;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

/// Mirrors the frontend's `MobileConnectionState` (`src/routes/mobile/mobile-tauri.ts`) exactly
/// — `reason` is `None` unless `connected` is false AND the disconnect is one of the three
/// distinguishable cases (m5): `"unreachable"`, `"auth_rejected"`, `"re_pair"`.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionState {
    pub paired: bool,
    pub connected: bool,
    pub reason: Option<String>,
}

impl ConnectionState {
    pub fn unpaired() -> Self {
        Self {
            paired: false,
            connected: false,
            reason: None,
        }
    }

    /// The `mobile-kill-state` event payload shape (`{ on: bool }`) — a tiny associated helper
    /// so `bridge.rs` doesn't need its own ad hoc struct for one field.
    pub fn kill_payload(on: bool) -> serde_json::Value {
        serde_json::json!({ "on": on })
    }
}

pub struct AppState {
    /// `Some` once paired and the client loop has been spawned — the loop itself may still be
    /// mid-backoff/disconnected; presence here means "this device knows how to reach the
    /// desktop", not "is live right now" (see `ConnectionState::connected` for that).
    pub client: Mutex<Option<ClientHandle>>,
    /// Flips the client loop's foreground/background gate (researcher-01) — the app's own
    /// lifecycle hooks call `.send(true/false)`; starts `true` since a freshly-launched app is
    /// definitionally foreground.
    pub foreground_tx: watch::Sender<bool>,
    pub connection: Mutex<ConnectionState>,
    pub data_dir: PathBuf,
    /// `FetchSession` request/response correlation (M7): `mobile_fetch_session` registers a
    /// sender here keyed by `session_id` before sending the frame; `bridge.rs` resolves it when
    /// the matching `SessionSnapshot` frame arrives.
    pub pending_snapshots: DashMap<Uuid, oneshot::Sender<SessionSnapshot>>,
    /// Last `HelloAck.epoch` seen — lets `bridge.rs` detect a server restart (C4) across
    /// reconnects and fire `mobile-resync-needed` (M7). `None` until the first `HelloAck`.
    pub last_epoch: Mutex<Option<u64>>,
}

impl AppState {
    pub fn new(data_dir: PathBuf, paired: bool) -> Self {
        // The receiver half is deliberately not kept here — `foreground_tx.subscribe()` mints
        // a fresh one (seeded with the CURRENT value) each time `commands::connect_and_spawn`
        // needs one, which is the correct pattern for a value that may be (re)connected
        // multiple times across one process's life (re-pair after unpair, etc.).
        let (foreground_tx, _initial_rx) = watch::channel(true);
        let mut connection = ConnectionState::unpaired();
        connection.paired = paired;
        Self {
            client: Mutex::new(None),
            foreground_tx,
            connection: Mutex::new(connection),
            data_dir,
            pending_snapshots: DashMap::new(),
            last_epoch: Mutex::new(None),
        }
    }
}
