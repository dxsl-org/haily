//! App-level voice commands + the `haily-chunk` → TTS-chunker wiring (Mobile Thin-Client plan
//! phase 4). Svelte NEVER calls `tauri-plugin-haily-voice`'s own commands directly for anything
//! press/release/speak-related — it calls the commands in THIS file instead, so the m4 contract
//! (press MUST stop TTS + pause the chunker feed before the recognizer starts) is enforced in
//! exactly one place, driven by [`crate::voice_state::PushToTalkGate`], rather than relying on
//! the frontend to always call things in the right order. `check_permissions`/
//! `request_permissions` have no such ordering concern and are thin pass-throughs.
use crate::state::AppState;
use crate::voice_state::GateAction;
use haily_types::{ClientFrame, DepthMode, ResponseChunk};
use serde::Serialize;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

/// Host-safe mirror of the plugin's own `PermissionStatus` — defined here (not re-exported from
/// `tauri_plugin_haily_voice`) because that crate is only ever a dependency on
/// `target_os = "android"` builds; a type from it can't appear in a signature this file compiles
/// unconditionally.
#[derive(Debug, Clone, Serialize)]
pub struct VoicePermissionStatus {
    pub microphone: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct VoiceTtsState {
    pub speaking: bool,
}

#[cfg(target_os = "android")]
mod plugin {
    use super::VoicePermissionStatus;
    use tauri::AppHandle;
    use tauri_plugin_haily_voice::{HailyVoiceExt, PermissionState, SpeakChunkArgs};

    fn to_dto(status: tauri_plugin_haily_voice::PermissionStatus) -> VoicePermissionStatus {
        let microphone = match status.microphone {
            PermissionState::Granted => "granted",
            PermissionState::Denied => "denied",
            PermissionState::Prompt => "prompt",
        };
        VoicePermissionStatus {
            microphone: microphone.to_string(),
        }
    }

    pub fn start_stt(app: &AppHandle) -> Result<(), String> {
        app.haily_voice().start_stt().map_err(|e| e.to_string())
    }
    pub fn stop_stt(app: &AppHandle) -> Result<(), String> {
        app.haily_voice().stop_stt().map_err(|e| e.to_string())
    }
    pub fn stop_speaking(app: &AppHandle) -> Result<(), String> {
        app.haily_voice().stop_speaking().map_err(|e| e.to_string())
    }
    pub fn speak_chunk(app: &AppHandle, text: String) -> Result<(), String> {
        app.haily_voice()
            .speak_chunk(SpeakChunkArgs { text })
            .map_err(|e| e.to_string())
    }
    pub fn tts_state(app: &AppHandle) -> Result<super::VoiceTtsState, String> {
        app.haily_voice()
            .tts_state()
            .map(|r| super::VoiceTtsState {
                speaking: r.speaking,
            })
            .map_err(|e| e.to_string())
    }
    pub fn check_permissions(app: &AppHandle) -> Result<VoicePermissionStatus, String> {
        app.haily_voice()
            .check_permissions()
            .map(to_dto)
            .map_err(|e| e.to_string())
    }
    pub fn request_permissions(app: &AppHandle) -> Result<VoicePermissionStatus, String> {
        app.haily_voice()
            .request_permissions()
            .map(to_dto)
            .map_err(|e| e.to_string())
    }
}

/// Host/dev-preview build has no voice hardware to invoke (mirrors `commands.rs`'s
/// `confirm_biometric` host-stub precedent) — every action is a safe, honest no-op/inert default,
/// never a fabricated success or capability.
#[cfg(not(target_os = "android"))]
mod plugin {
    use super::{VoicePermissionStatus, VoiceTtsState};
    use tauri::AppHandle;

    pub fn start_stt(_app: &AppHandle) -> Result<(), String> {
        Ok(())
    }
    pub fn stop_stt(_app: &AppHandle) -> Result<(), String> {
        Ok(())
    }
    pub fn stop_speaking(_app: &AppHandle) -> Result<(), String> {
        Ok(())
    }
    pub fn speak_chunk(_app: &AppHandle, _text: String) -> Result<(), String> {
        Ok(())
    }
    pub fn tts_state(_app: &AppHandle) -> Result<VoiceTtsState, String> {
        Ok(VoiceTtsState { speaking: false })
    }
    pub fn check_permissions(_app: &AppHandle) -> Result<VoicePermissionStatus, String> {
        Ok(VoicePermissionStatus {
            microphone: "denied".to_string(),
        })
    }
    pub fn request_permissions(_app: &AppHandle) -> Result<VoicePermissionStatus, String> {
        Ok(VoicePermissionStatus {
            microphone: "denied".to_string(),
        })
    }
}

fn resume_gate(state: &AppState) {
    let _ = state
        .voice
        .gate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .on_stt_ended();
}

/// Push-to-talk pressed (m4): runs the gate transition FIRST, executing every action it returns,
/// THEN starts the recognizer — `StopSpeaking` always fires before `start_stt` is ever called.
#[tauri::command]
pub async fn voice_ptt_start(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let actions = state
        .voice
        .gate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .on_ptt_pressed();
    for action in actions {
        if action == GateAction::StopSpeaking {
            plugin::stop_speaking(&app)?;
        }
        // `PauseChunkerFeed`/`ResumeChunkerFeed` need no separate effect here — `handle_tts_chunk`
        // reads the gate's own state directly (`is_chunker_paused`), so the gate transition IS
        // the pause; these variants exist for the FSM's own testability/documentation.
    }
    plugin::start_stt(&app)
}

/// Push-to-talk released: asks the recognizer to finalize. Does NOT resume the chunker feed —
/// that only happens once a real outcome (final transcript or error) is observed, since Android
/// keeps processing buffered audio briefly after this call.
#[tauri::command]
pub async fn voice_ptt_stop(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state
        .voice
        .gate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .on_ptt_released();
    plugin::stop_stt(&app)
}

/// The recognizer produced a final transcript: resumes the chunker feed AND sends it as a normal
/// chat message using the SAME caller-supplied-`session_id` path as `mobile_send_message` (the
/// caller pre-registers `sessionIndex` client-side first, exactly like the typed-text send flow).
#[tauri::command]
pub async fn voice_send_transcript(
    session_id: String,
    text: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    resume_gate(&state);
    let session_id = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    let handle = crate::commands::client_handle(&state)?;
    if !handle.send(ClientFrame::UserMessage {
        session_id,
        message: text,
        depth: DepthMode::Normal,
    }) {
        return Err("not connected — message was not sent".to_string());
    }
    Ok(())
}

/// The recognizer ended WITHOUT a usable transcript (error, no-match, or an explicit cancel) —
/// resumes the chunker feed only; nothing to send.
#[tauri::command]
pub async fn voice_stt_cancelled(state: State<'_, AppState>) -> Result<(), String> {
    resume_gate(&state);
    Ok(())
}

/// Whether the gate currently has the mic session open — used by the UI to restore its
/// recording indicator after e.g. an app resume, without relying purely on its own local
/// press/release event history (which a backgrounded-then-foregrounded webview can lose).
#[tauri::command]
pub async fn voice_ptt_is_listening(state: State<'_, AppState>) -> Result<bool, String> {
    let listening = state
        .voice
        .gate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .state()
        == crate::voice_state::VoiceState::Listening;
    Ok(listening)
}

#[tauri::command]
pub async fn voice_set_tts_enabled(on: bool, state: State<'_, AppState>) -> Result<(), String> {
    state.voice.tts_enabled.store(on, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
pub async fn voice_tts_state(app: AppHandle) -> Result<VoiceTtsState, String> {
    plugin::tts_state(&app)
}

#[tauri::command]
pub async fn voice_check_permissions(app: AppHandle) -> Result<VoicePermissionStatus, String> {
    plugin::check_permissions(&app)
}

#[tauri::command]
pub async fn voice_request_permissions(app: AppHandle) -> Result<VoicePermissionStatus, String> {
    plugin::request_permissions(&app)
}

/// Called by `bridge.rs` for every `ServerBody::Chunk` frame: feeds `Text` deltas into the shared
/// sentence chunker and speaks completed sentences; `Complete` flushes the remainder. A no-op
/// while TTS is toggled off or the m4 gate has the feed paused — in the paused case the delta is
/// simply not accumulated (the OLD turn's remaining speech is intentionally abandoned, matching
/// the `stop_speaking` flush that already happened on press; a NEW turn starts its own fresh text
/// once the mic session ends).
pub fn handle_tts_chunk(app: &AppHandle, chunk: &ResponseChunk) {
    let state = app.state::<AppState>();
    // A turn-ending Error discards the buffered partial UNCONDITIONALLY (before the tts-enabled/
    // paused early-returns): its haily-types contract is "discard the partial buffer", and a
    // stale tail left behind here would be glued onto — and spoken before — the NEXT turn's text.
    if matches!(chunk, ResponseChunk::Error(_)) {
        state
            .voice
            .chunker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        return;
    }
    if !state.voice.tts_enabled.load(Ordering::Relaxed) {
        return;
    }
    if state
        .voice
        .gate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_chunker_paused()
    {
        return;
    }
    let sentences = {
        let mut chunker = state
            .voice
            .chunker
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match chunk {
            ResponseChunk::Text(delta) => chunker.push(delta),
            ResponseChunk::Complete => chunker.flush().into_iter().collect(),
            _ => Vec::new(),
        }
    };
    for sentence in sentences {
        let _ = plugin::speak_chunk(app, sentence);
    }
}
