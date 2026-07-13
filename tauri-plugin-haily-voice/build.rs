// Standard Tauri v2 plugin build script: generates the ACL permission schema (`permissions/`)
// from `COMMANDS` and points the mobile bridge at `android/` (see `src/mobile.rs`'s
// `run_mobile_plugin` calls, which look up Kotlin methods by these exact names).
const COMMANDS: &[&str] = &[
    "start_stt",
    "stop_stt",
    "speak_chunk",
    "stop_speaking",
    "tts_state",
    "check_permissions",
    "request_permissions",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .build();
}
