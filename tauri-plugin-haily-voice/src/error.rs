use serde::{ser::Serializer, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

/// Every failure mode a command in this plugin can surface to the JS `invoke()` caller.
/// Deliberately flat (no nested platform-specific variants) — the Kotlin side already collapses
/// its own richer `RecognizerListener`/`TextToSpeech` error codes into a short reason string
/// before it ever reaches Rust (see `HailyVoicePlugin.kt`'s `Invoke.reject` call sites).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Tauri(#[from] tauri::Error),
    #[error("plugin invocation failed: {0}")]
    PluginInvoke(String),
    #[error("this platform has no voice plugin implementation (Android only in this phase)")]
    Unsupported,
}

// `tauri::command` requires the error type to serialize, so the JS-visible message is exactly
// this variant's `Display` output — no internal detail beyond what's already user-facing.
impl Serialize for Error {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.to_string().as_ref())
    }
}
