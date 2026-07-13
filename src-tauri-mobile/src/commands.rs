//! Tauri commands. `send_message`/`approve_tool` are DELIBERATELY named identically to the
//! desktop's own commands (`src-tauri/src/lib.rs`) — see `src/routes/mobile/mobile-tauri.ts`'s
//! module doc for why that lets `ApprovalModal.svelte`/`ProactivePanel.svelte` work unmodified.
use crate::state::{AppState, ConnectionState};
use crate::vault::{self, StoredPairing};
use crate::{bridge, pairing};
use haily_mobile_client::{spawn as spawn_client, ClientHandle, MobileClientConfig};
use haily_types::{ClientFrame, DepthMode, PairingQr, SessionSnapshot};
use std::time::Duration;
use tauri::{AppHandle, State};
use tokio::sync::oneshot;
use uuid::Uuid;

const FETCH_SESSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Invokes the OS biometric/passcode prompt before an `Approve{approved:true}` frame is sent
/// (M1) — the server, not this check, is the actual enforcement point
/// (`mobile_approval_policy`); this only decides what `biometric_ok` reports. Always attempted
/// (not just for tools the client believes are High/IrreversibleWrite) because the wire alone
/// doesn't reliably tell the client a prompt's risk tier — always gating is simpler and never
/// under-reports `biometric_ok` for a tier that needed it.
#[cfg(any(target_os = "android", target_os = "ios"))]
fn confirm_biometric(app: &AppHandle, reason: String) -> bool {
    use tauri_plugin_biometric::BiometricExt;
    app.biometric()
        .authenticate(reason, Default::default())
        .is_ok()
}

/// Host/dev-preview build has no biometric hardware to invoke — reports `false` so
/// `approve_tool` never fabricates a `biometric_ok: true` it can't back up.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn confirm_biometric(_app: &AppHandle, _reason: String) -> bool {
    false
}

/// Shared by `mobile_pair` (fresh pairing) and the startup auto-reconnect (`lib.rs`'s
/// `setup`): spawns the WS client loop plus its event-forwarding bridge.
pub fn connect_and_spawn(app: &AppHandle, state: &AppState, qr: PairingQr, token: String) {
    let foreground_rx = state.foreground_tx.subscribe();
    let (handle, events) = spawn_client(MobileClientConfig { qr, token }, foreground_rx);
    *state.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    bridge::spawn(app.clone(), events);
}

/// `pub(crate)` (not just module-private) so `voice.rs`'s `voice_send_transcript` command can
/// reuse the exact same "resolve the connected client or a uniform not-connected error" logic
/// rather than re-implementing it.
pub(crate) fn client_handle(state: &AppState) -> Result<ClientHandle, String> {
    state
        .client
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| "not paired/connected".to_string())
}

#[tauri::command]
pub async fn mobile_status(state: State<'_, AppState>) -> Result<ConnectionState, String> {
    Ok(state
        .connection
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone())
}

/// Redeems `qr`'s pairing code (blocks server-side until the desktop's OOB confirm resolves it,
/// M4), persists the token+QR to the vault, then connects.
#[tauri::command]
pub async fn mobile_pair(
    qr: PairingQr,
    device_name: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let response = pairing::redeem(&qr, &device_name)
        .await
        .map_err(|e| e.to_string())?;

    let data_dir = state.data_dir.clone();
    let stored = StoredPairing {
        token: response.device_token.clone(),
        qr: qr.clone(),
    };
    tokio::task::spawn_blocking(move || vault::save_pairing(&data_dir, &stored))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

    state
        .connection
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .paired = true;
    connect_and_spawn(&app, &state, qr, response.device_token);
    Ok(())
}

/// Forgets this pairing (M5): disconnects, wipes the vault, resets connection state.
#[tauri::command]
pub async fn mobile_unpair(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(client) = state
        .client
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        client.disconnect();
    }
    let data_dir = state.data_dir.clone();
    tokio::task::spawn_blocking(move || vault::clear_pairing(&data_dir))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    *state.connection.lock().unwrap_or_else(|e| e.into_inner()) = ConnectionState::unpaired();
    Ok(())
}

/// Same command name/signature as the desktop GUI (`src-tauri/src/lib.rs::send_message`) — see
/// the module doc. Mints the session id server-side; used by `ProactivePanel.svelte` (shared,
/// reused unmodified) for its fire-and-forget "view reminder"/"request undo" replies, where no
/// caller needs the id in advance. `MobileChat.svelte`'s OWN send button uses
/// [`mobile_send_message`] instead, which takes a caller-supplied id.
#[tauri::command]
pub async fn send_message(message: String, state: State<'_, AppState>) -> Result<String, String> {
    let handle = client_handle(&state)?;
    let session_id = Uuid::new_v4();
    let sent = handle.send(ClientFrame::UserMessage {
        session_id,
        message,
        depth: DepthMode::Normal,
    });
    if !sent {
        return Err("not connected — message was not sent".to_string());
    }
    Ok(session_id.to_string())
}

/// Like [`send_message`] but takes a CALLER-SUPPLIED `session_id` (review finding: pre-register
/// `sessionIndex` before this command resolves). `MobileChat.svelte`'s send button mints the id
/// client-side, registers its `sessionIndex` entry, THEN calls this — closing the race where a
/// `haily-chunk` event for the new session could otherwise arrive over IPC before this command's
/// return value does (Tauri events and command results are independent channels with no
/// ordering guarantee between them).
#[tauri::command]
pub async fn mobile_send_message(
    session_id: String,
    message: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let session_id = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    let handle = client_handle(&state)?;
    if !handle.send(ClientFrame::UserMessage {
        session_id,
        message,
        depth: DepthMode::Normal,
    }) {
        return Err("not connected — message was not sent".to_string());
    }
    Ok(())
}

/// Cancels `session_id`'s in-flight turn (phase 3 review amendment — `ClientFrame::CancelTurn`,
/// additive per §9 of `docs/mobile-protocol.md`). Same name/param shape as the desktop GUI's own
/// `cancel_turn` command, so `$lib/tauri.ts`'s existing `cancelTurn` helper works here
/// unmodified (see `MobileChat.svelte`'s Stop button). Returns `false` (not an error) if not
/// connected — mirrors every other best-effort `handle.send` result in this file.
#[tauri::command]
pub async fn cancel_turn(session_id: String, state: State<'_, AppState>) -> Result<bool, String> {
    let session_id = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    let handle = client_handle(&state)?;
    Ok(handle.send(ClientFrame::CancelTurn { session_id }))
}

/// Same command name as the desktop GUI — see the module doc. Runs the biometric gate (M1)
/// before forwarding an `approved: true` decision; a `false` decision skips it (nothing to gate
/// — see `docs/mobile-protocol.md`'s approval round-trip). If the user tapped Approve but the
/// biometric check failed/was cancelled, emits `mobile-approval-denied` (review finding 6c) —
/// `ApprovalModal.svelte` (shared, desktop-owned) closes its dialog either way with no notion of
/// this outcome, so a sibling listener (`+page.svelte`) surfaces the denial instead of letting
/// the UI silently imply the action went through.
#[tauri::command]
pub async fn approve_tool(
    session_id: String,
    approval_id: String,
    approved: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    let session_id = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    let approval_id = Uuid::parse_str(&approval_id).map_err(|e| e.to_string())?;
    let handle = client_handle(&state)?;
    let biometric_ok = if approved {
        confirm_biometric(&app, "Confirm this action in Haily".to_string())
    } else {
        false
    };
    if approved && !biometric_ok {
        use tauri::Emitter;
        let _ = app.emit(
            "mobile-approval-denied",
            serde_json::json!({ "approval_id": approval_id, "reason": "biometric_failed" }),
        );
    }
    Ok(handle.send(ClientFrame::Approve {
        approval_id,
        session_id,
        approved,
        biometric_ok,
    }))
}

/// ENABLE-ONLY from mobile (M1) — rejected client-side BEFORE it ever reaches the wire, mirroring
/// the server's own enforcement so a compromised/patched frontend still cannot disable safety
/// remotely from two independent layers.
#[tauri::command]
pub async fn mobile_set_kill_switch(
    session_id: String,
    on: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if !on {
        return Err(
            "mobile can only ENABLE the kill switch — disabling requires the desktop".to_string(),
        );
    }
    let session_id = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    let handle = client_handle(&state)?;
    if !handle.send(ClientFrame::SetKillSwitch {
        session_id,
        on: true,
    }) {
        return Err("not connected".to_string());
    }
    Ok(())
}

/// Requests a `SessionSnapshot` (M7) and awaits the matching response frame, correlated by
/// `session_id` via `AppState::pending_snapshots`. Times out rather than hanging forever if the
/// connection drops mid-request.
#[tauri::command]
pub async fn mobile_fetch_session(
    session_id: String,
    state: State<'_, AppState>,
) -> Result<SessionSnapshot, String> {
    let sid = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    let handle = client_handle(&state)?;
    let (tx, rx) = oneshot::channel();
    state.pending_snapshots.insert(sid, tx);
    if !handle.send(ClientFrame::FetchSession { session_id: sid }) {
        state.pending_snapshots.remove(&sid);
        return Err("not connected".to_string());
    }
    match tokio::time::timeout(FETCH_SESSION_TIMEOUT, rx).await {
        Ok(Ok(snapshot)) => Ok(snapshot),
        Ok(Err(_)) => Err("session snapshot request was dropped".to_string()),
        Err(_) => {
            state.pending_snapshots.remove(&sid);
            Err("timed out waiting for the session snapshot".to_string())
        }
    }
}
