use serde::{Deserialize, Serialize};

/// One TTS-ready sentence to enqueue (`speak_chunk`) — see
/// `haily_mobile_client::TtsChunker::push`, which is what produces these on the app side. Always
/// a single complete sentence: Android `TextToSpeech`/iOS `AVSpeechSynthesizer` both queue whole
/// utterance strings, never partial tokens (researcher-02).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakChunkArgs {
    pub text: String,
}

/// Android's runtime-permission tri-state, mirrored 1:1 (never collapsed to a bool) so the UI can
/// distinguish "never asked yet" from "the user said no" and only show its own rationale copy in
/// the latter case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionState {
    Granted,
    Denied,
    Prompt,
}

/// `check_permissions`/`request_permissions`'s shared response shape — one field per permission
/// this plugin actually needs. Only `microphone` (`RECORD_AUDIO`) exists today; kept as a struct
/// rather than a bare enum so a future permission (unlikely — TTS needs none) is additive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionStatus {
    pub microphone: PermissionState,
}

/// `tts_state`'s response — whether the synthesizer is actively speaking right now. Used by the
/// mobile UI's speaker-icon animation and, indirectly, by the m4 gate (which doesn't need to
/// query this to decide whether to call `stop_speaking` — it always does, unconditionally — but
/// the UI benefits from knowing).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TtsStateResponse {
    pub speaking: bool,
}
