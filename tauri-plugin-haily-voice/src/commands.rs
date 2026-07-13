//! Thin `#[tauri::command]` wrappers — each one just forwards to the `HailyVoiceExt` handle
//! Tauri's managed state holds. `src-tauri-mobile` does NOT call these directly from Svelte for
//! `start_stt`/`stop_stt`/`speak_chunk`/`stop_speaking`; its own app-level `voice.rs` commands
//! wrap these instead so the m4 press/pause ordering is enforced in ONE place (see that module's
//! doc comment). `check_permissions`/`request_permissions` have no such ordering concern and may
//! be invoked directly.
use tauri::{command, AppHandle, Runtime};

use crate::models::{PermissionStatus, SpeakChunkArgs, TtsStateResponse};
use crate::{HailyVoiceExt, Result};

#[command]
pub(crate) async fn start_stt<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    app.haily_voice().start_stt()
}

#[command]
pub(crate) async fn stop_stt<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    app.haily_voice().stop_stt()
}

#[command]
pub(crate) async fn speak_chunk<R: Runtime>(
    app: AppHandle<R>,
    payload: SpeakChunkArgs,
) -> Result<()> {
    app.haily_voice().speak_chunk(payload)
}

#[command]
pub(crate) async fn stop_speaking<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    app.haily_voice().stop_speaking()
}

#[command]
pub(crate) async fn tts_state<R: Runtime>(app: AppHandle<R>) -> Result<TtsStateResponse> {
    app.haily_voice().tts_state()
}

#[command]
pub(crate) async fn check_permissions<R: Runtime>(app: AppHandle<R>) -> Result<PermissionStatus> {
    app.haily_voice().check_permissions()
}

#[command]
pub(crate) async fn request_permissions<R: Runtime>(app: AppHandle<R>) -> Result<PermissionStatus> {
    app.haily_voice().request_permissions()
}
