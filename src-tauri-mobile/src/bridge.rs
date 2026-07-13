//! Forwards `haily_mobile_client::ClientEvent`s onto Tauri `emit`s under the SAME event names
//! the desktop GUI uses (`haily-chunk`, `haily-run-events`, `haily-proactive-cards`) plus mobile-
//! only ones (`mobile-connection-state`, `mobile-kill-state`, `mobile-resync-needed`) — the one
//! place that decides what each `ServerBody` variant means for the UI (Architecture: "IPC
//! bridge: decode `ServerFrame` … → `emit`s mapped events").
use crate::state::{AppState, ConnectionState};
use haily_mobile_client::{ClientEvent, StopReason};
use haily_types::{MobileError, ServerBody};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

pub fn spawn(app: AppHandle, mut events: mpsc::Receiver<ClientEvent>) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                ClientEvent::Connected { .. } => set_connection(&app, true, None),
                // Network-shaped only (review finding: typed connect errors) — `PinMismatch`/
                // `AuthRejected` now arrive as `Stopped` below instead, never here.
                ClientEvent::Disconnected { .. } => {
                    set_connection(&app, false, Some("unreachable"))
                }
                ClientEvent::Stopped { reason } => {
                    let mapped = match reason {
                        StopReason::PinMismatch => "re_pair",
                        StopReason::AuthRejected => "auth_rejected",
                    };
                    set_connection(&app, false, Some(mapped));
                }
                ClientEvent::Frame(frame) => handle_frame(&app, frame.body),
            }
        }
    });
}

fn set_connection(app: &AppHandle, connected: bool, reason: Option<&str>) {
    let state = app.state::<AppState>();
    let snapshot = {
        let mut conn = state.connection.lock().unwrap_or_else(|e| e.into_inner());
        conn.connected = connected;
        conn.reason = reason.map(str::to_string);
        conn.clone()
    };
    let _ = app.emit("mobile-connection-state", snapshot);
}

/// Records `epoch` as the last-seen one and returns `true` if a DIFFERENT epoch was previously
/// recorded (C4 — the server restarted, so the seq-space this client had been tracking is now
/// meaningless). `false` on the very first `HelloAck` (nothing to compare against yet).
fn epoch_changed(app: &AppHandle, epoch: u64) -> bool {
    let state = app.state::<AppState>();
    let mut last = state.last_epoch.lock().unwrap_or_else(|e| e.into_inner());
    let changed = matches!(*last, Some(prev) if prev != epoch);
    *last = Some(epoch);
    changed
}

/// Emits the resync-needed signal (M7): the frontend's job is to re-`FetchSession` whatever
/// session(s) it currently has open and replace their local view wholesale (§6.3) — this bridge
/// has no session-open bookkeeping of its own to drive that directly (only the Svelte layer
/// tracks "which session is the user looking at"), so a dedicated event is the design this
/// bridge already supports best (mirrors every other `emit`-then-frontend-acts wiring here).
fn emit_resync_needed(app: &AppHandle) {
    let _ = app.emit("mobile-resync-needed", ());
}

fn handle_frame(app: &AppHandle, body: ServerBody) {
    match body {
        ServerBody::HelloAck { epoch, kill_on, .. } => {
            set_connection(app, true, None);
            let _ = app.emit("mobile-kill-state", ConnectionState::kill_payload(kill_on));
            // C4/M7: an epoch change on reconnect means the server restarted — the seq cursor
            // was already reset by `haily-mobile-client`'s dedup layer, but the APPLICATION
            // state (transcript/run-status the UI is showing) is now stale relative to the
            // ground truth and must be re-fetched, not just left as-is.
            if epoch_changed(app, epoch) {
                emit_resync_needed(app);
            }
        }
        ServerBody::Chunk { session_id, chunk } => {
            crate::voice::handle_tts_chunk(app, &chunk);
            let _ = app.emit(
                "haily-chunk",
                serde_json::json!({ "session_id": session_id, "chunk": chunk }),
            );
        }
        ServerBody::Run { session_id, event } => {
            let _ = app.emit(
                "haily-run-events",
                serde_json::json!({ "session_id": session_id, "event": event }),
            );
        }
        ServerBody::ProactiveList(cards) => {
            let _ = app.emit("haily-proactive-cards", cards);
        }
        // v1 has no dedicated notification surface on mobile (no persistent card panel yet,
        // mirrors `MobileAdapter::notify`'s own desktop-side "no mobile-v1 surface" note for
        // `WorkItemsChanged`) — a daemon-wide notification reaches the phone the next time it
        // calls `FetchProactive`/`FetchSession`, not as a live push. Documented scope-out, not
        // a silently dropped requirement.
        ServerBody::Notify(_) => {}
        ServerBody::SessionSnapshot(snapshot) => {
            let state = app.state::<AppState>();
            if let Some((_, sender)) = state.pending_snapshots.remove(&snapshot.session_id) {
                let _ = sender.send(snapshot);
            }
            // No pending waiter — either a resend the resync path will pick up via its own
            // explicit `FetchSession` call, or a genuinely unsolicited frame. Either way there is
            // nothing to correlate it to here.
        }
        ServerBody::KillState { on } => {
            let _ = app.emit("mobile-kill-state", ConnectionState::kill_payload(on));
        }
        ServerBody::Error(MobileError::AuthRejected) => {
            set_connection(app, false, Some("auth_rejected"))
        }
        ServerBody::Error(MobileError::ProtocolVersion) => {
            let state = app.state::<AppState>();
            if let Some(client) = state
                .client
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                client.disconnect();
            }
            set_connection(app, false, Some("re_pair"));
        }
        // M7: the ring-buffer window was exceeded — the SAME recovery as an epoch change
        // (re-`FetchSession` whatever is open), just triggered by a different server signal.
        ServerBody::Error(MobileError::ResumeWindowExceeded) => emit_resync_needed(app),
        ServerBody::Error(other) => {
            tracing::debug!("mobile: post-connect error frame: {other:?}");
        }
        ServerBody::Pong => {}
        ServerBody::Unknown { type_tag } => {
            tracing::debug!(
                type_tag,
                "mobile: unrecognized server frame — inert placeholder"
            );
        }
    }
}
