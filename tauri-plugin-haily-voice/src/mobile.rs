//! Android bridge (iOS deferred to a later phase per the plan's Next Steps — Swift port is not
//! attempted here). `PluginHandle::run_mobile_plugin`'s first argument is the Kotlin `@Command`
//! method name, looked up by EXACT string match via reflection — every string below must match
//! `android/src/main/java/io/haily/voice/HailyVoicePlugin.kt` one-to-one; there is no
//! compile-time check tying the two together, so a rename on one side silently breaks the other
//! at runtime instead of at build time.
use serde::de::DeserializeOwned;
use tauri::{
    plugin::{PluginApi, PluginHandle},
    AppHandle, Runtime,
};

use crate::models::{PermissionStatus, SpeakChunkArgs, TtsStateResponse};
use crate::{Error, Result};

const PLUGIN_IDENTIFIER: &str = "io.haily.voice";

pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> Result<HailyVoice<R>> {
    let handle = api.register_android_plugin(PLUGIN_IDENTIFIER, "HailyVoicePlugin")?;
    Ok(HailyVoice(handle))
}

/// Rust-side handle to the live Kotlin plugin instance. One instance lives in Tauri's managed
/// state for the app's lifetime (`lib.rs`'s `setup`); every command in `commands.rs` goes through
/// this handle rather than holding its own reference.
pub struct HailyVoice<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> HailyVoice<R> {
    fn invoke<T: serde::Serialize, Resp: DeserializeOwned>(
        &self,
        method: &str,
        payload: T,
    ) -> Result<Resp> {
        self.0
            .run_mobile_plugin(method, payload)
            .map_err(|e| Error::PluginInvoke(format!("{method}: {e}")))
    }

    /// Begins push-to-talk capture. The CALLER (the app-level `voice_ptt_start` command, not this
    /// plugin) is responsible for the m4 ordering — stopping TTS and pausing the chunker feed
    /// BEFORE this is ever invoked; this method has no opinion on that sequencing, it only starts
    /// the recognizer.
    pub fn start_stt(&self) -> Result<()> {
        self.invoke("startStt", ())
    }

    /// Asks the recognizer to finalize. Does NOT itself confirm the session has ended — the
    /// Kotlin side keeps processing buffered audio briefly and reports the real outcome via a
    /// `voice-stt-final`/`voice-stt-error` event (see `HailyVoicePlugin.kt`), not this call's
    /// return.
    pub fn stop_stt(&self) -> Result<()> {
        self.invoke("stopStt", ())
    }

    /// Enqueues one complete, TTS-ready sentence (`QUEUE_ADD`). Never call with a partial/token-
    /// level fragment — Android's `TextToSpeech` only accepts whole utterance strings.
    pub fn speak_chunk(&self, payload: SpeakChunkArgs) -> Result<()> {
        self.invoke("speakChunk", payload)
    }

    /// Immediately stops and flushes the synthesizer's queue (`QUEUE_FLUSH`/stop) — used for
    /// barge-in and for the m4 press-time flush.
    pub fn stop_speaking(&self) -> Result<()> {
        self.invoke("stopSpeaking", ())
    }

    pub fn tts_state(&self) -> Result<TtsStateResponse> {
        self.invoke("ttsState", ())
    }

    pub fn check_permissions(&self) -> Result<PermissionStatus> {
        self.invoke("checkPermissions", ())
    }

    pub fn request_permissions(&self) -> Result<PermissionStatus> {
        self.invoke("requestPermissions", ())
    }
}
