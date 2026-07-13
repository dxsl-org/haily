//! Mobile Thin-Client plan phase 3 — the mobile Tauri shell's thin lib.rs. The WebView never
//! opens a socket of its own (M14, enforced by this app's restrictive CSP in `tauri.conf.json`
//! together with the CI grep-guard); the actual WS client lives entirely in
//! `haily-mobile-client` and is driven from here via Tauri commands and the `bridge` module's
//! `emit`s.
mod bridge;
mod commands;
mod pairing;
mod state;
mod vault;
mod voice;
mod voice_state;

use state::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // `mut` is only exercised on `target_os = "android"/"ios"` (the plugin-registration block
    // below); a host `cargo check` build never reassigns it — `#[allow]` rather than splitting
    // this into two differently-shaped builder chains per target.
    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default();
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        builder = builder
            .plugin(tauri_plugin_barcode_scanner::init())
            .plugin(tauri_plugin_biometric::init());
    }
    // Android-only (phase 4): iOS's voice half is an explicitly deferred follow-up (the plan's
    // Next Steps), so this plugin is gated narrower than barcode-scanner/biometric above.
    #[cfg(target_os = "android")]
    {
        builder = builder.plugin(tauri_plugin_haily_voice::init());
    }

    builder
        .setup(|app| {
            let data_dir = app
                .path()
                .app_local_data_dir()
                .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;
            std::fs::create_dir_all(&data_dir)?;

            // Blocking (file + KDF), but tiny (single small vault, one KDF pass) and this is
            // the app's one-time startup path — mirrors the desktop shell's own synchronous
            // `setup()`-time bootstrap work.
            let stored = vault::load_pairing(&data_dir)
                .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;
            let paired = stored.is_some();
            app.manage(AppState::new(data_dir, paired));

            if let Some(pairing) = stored {
                let state = app.state::<AppState>();
                commands::connect_and_spawn(app.handle(), state.inner(), pairing.qr, pairing.token);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::mobile_status,
            commands::mobile_pair,
            commands::mobile_unpair,
            commands::send_message,
            commands::mobile_send_message,
            commands::cancel_turn,
            commands::approve_tool,
            commands::mobile_set_kill_switch,
            commands::mobile_fetch_session,
            voice::voice_ptt_start,
            voice::voice_ptt_stop,
            voice::voice_send_transcript,
            voice::voice_stt_cancelled,
            voice::voice_ptt_is_listening,
            voice::voice_set_tts_enabled,
            voice::voice_tts_state,
            voice::voice_check_permissions,
            voice::voice_request_permissions,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Haily Mobile");
}
