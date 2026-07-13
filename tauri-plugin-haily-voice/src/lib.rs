//! `tauri-plugin-haily-voice` — OS-native push-to-talk STT + streaming sentence-chunked TTS for
//! the Haily mobile client (Mobile Thin-Client plan phase 4). Android only in this phase; iOS is
//! an explicitly deferred follow-up (plan's Next Steps). This crate is added to
//! `src-tauri-mobile/Cargo.toml` under `[target.'cfg(target_os = "android")'.dependencies]` —
//! never a host dependency, so a Windows `cargo check`/`cargo test` from `src-tauri-mobile` never
//! attempts to compile it at all (mirrors `tauri-plugin-barcode-scanner`/`tauri-plugin-biometric`'s
//! existing target-gate precedent in that crate's own `Cargo.toml`).
//!
//! Module map:
//! - [`mobile`] — the Android `PluginHandle` bridge; `run_mobile_plugin` calls into
//!   `android/src/main/java/io/haily/voice/HailyVoicePlugin.kt` by method name.
//! - [`commands`] — the `#[tauri::command]` surface Tauri's `invoke_handler!` registers.
//! - [`models`] — wire payload/response shapes shared by both sides.
//! - [`error`] — this plugin's flat `Error`/`Result`.
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

mod commands;
mod error;
mod mobile;
mod models;

pub use error::{Error, Result};
pub use models::{PermissionState, PermissionStatus, SpeakChunkArgs, TtsStateResponse};

use mobile::HailyVoice;

/// Extension trait giving any `AppHandle`/`App`/`Window` access to `app.haily_voice()` — mirrors
/// the `<Name>Ext` convention every other Tauri mobile plugin uses.
pub trait HailyVoiceExt<R: Runtime> {
    fn haily_voice(&self) -> &HailyVoice<R>;
}

impl<R: Runtime, T: Manager<R>> HailyVoiceExt<R> for T {
    fn haily_voice(&self) -> &HailyVoice<R> {
        self.state::<HailyVoice<R>>().inner()
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("haily-voice")
        .invoke_handler(tauri::generate_handler![
            commands::start_stt,
            commands::stop_stt,
            commands::speak_chunk,
            commands::stop_speaking,
            commands::tts_state,
            commands::check_permissions,
            commands::request_permissions,
        ])
        .setup(|app, api| {
            let haily_voice = mobile::init(app, api)?;
            app.manage(haily_voice);
            Ok(())
        })
        .build()
}
