package io.haily.voice

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.speech.RecognitionListener
import android.speech.RecognizerIntent
import android.speech.SpeechRecognizer
import android.speech.tts.TextToSpeech
import android.speech.tts.UtteranceProgressListener
import android.webkit.WebView
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.util.Locale

/**
 * Push-to-talk STT (`SpeechRecognizer`, network-primary with a best-effort on-device branch on
 * API 33+) and streaming sentence-chunked TTS (`TextToSpeech`, `QUEUE_ADD`/`QUEUE_FLUSH`) —
 * researcher-02's documented shape. The m4 press/pause CONTRACT (stop TTS + mute the sentence
 * feed before starting to listen) is enforced one layer up, in `src-tauri-mobile`'s own
 * `voice.rs` — this class only starts/stops the recognizer and synthesizer on request; it has no
 * opinion on when those requests should happen relative to each other.
 *
 * `trigger(event, data)` pushes an unprompted event straight to the JS side (Tauri's Android
 * plugin base class forwards it through the normal Tauri event bus) — used for the three
 * asynchronous STT callbacks and the two TTS utterance-completion callbacks, none of which are
 * responses to a specific Rust-initiated call.
 */
@TauriPlugin(
    permissions = [
        Permission(strings = [Manifest.permission.RECORD_AUDIO], alias = "microphone"),
    ],
)
class HailyVoicePlugin(private val activity: Activity) : Plugin(activity) {
    private var speechRecognizer: SpeechRecognizer? = null
    private var tts: TextToSpeech? = null

    // Written from the TTS engine's callback thread, read from the UI/command thread — @Volatile
    // guarantees cross-thread visibility (plain Boolean writes may otherwise never be observed).
    @Volatile
    private var ttsReady = false

    @Volatile
    private var speaking = false
    private val audioFocus = AudioFocus(activity).apply {
        onTtsShouldPause = { tts?.stop(); speaking = false }
    }

    override fun load(webView: WebView) {
        super.load(webView)
        tts = TextToSpeech(activity) { status ->
            if (status != TextToSpeech.SUCCESS) return@TextToSpeech
            // vi-VN primary; researcher-02 notes VN offline packs aren't guaranteed, but the
            // ENGINE itself still handles vi-VN via its own network path when needed — falling
            // back to the device default only if the locale is entirely unsupported.
            val result = tts?.setLanguage(Locale("vi", "VN"))
            if (result == TextToSpeech.LANG_MISSING_DATA || result == TextToSpeech.LANG_NOT_SUPPORTED) {
                tts?.setLanguage(Locale.getDefault())
            }
            ttsReady = true
            tts?.setOnUtteranceProgressListener(object : UtteranceProgressListener() {
                override fun onStart(utteranceId: String?) {
                    speaking = true
                }

                override fun onDone(utteranceId: String?) {
                    speaking = false
                    audioFocus.releaseTts()
                    trigger("voice-tts-done", JSObject().put("utteranceId", utteranceId))
                }

                @Deprecated("deprecated in the platform API; no non-deprecated overload exists yet")
                override fun onError(utteranceId: String?) {
                    speaking = false
                    audioFocus.releaseTts()
                    trigger(
                        "voice-tts-error",
                        JSObject().put("utteranceId", utteranceId).put("error", "tts-error"),
                    )
                }
            })
        }
    }

    @Command
    fun startStt(invoke: Invoke) {
        if (ContextCompat.checkSelfPermission(activity, Manifest.permission.RECORD_AUDIO) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            invoke.reject("RECORD_AUDIO permission not granted")
            return
        }
        if (!SpeechRecognizer.isRecognitionAvailable(activity)) {
            invoke.reject("no speech recognizer available on this device")
            return
        }
        activity.runOnUiThread {
            if (speechRecognizer == null) {
                speechRecognizer = createRecognizer()
            }
            audioFocus.requestCapture()
            speechRecognizer?.startListening(buildRecognizerIntent())
        }
        invoke.resolve()
    }

    /** Asks the recognizer to finalize. Does not itself confirm the session ended — the real
     * outcome always arrives via `voice-stt-final`/`voice-stt-error` from [createRecognizer]'s
     * listener, since Android keeps processing buffered audio briefly after this call returns. */
    @Command
    fun stopStt(invoke: Invoke) {
        activity.runOnUiThread { speechRecognizer?.stopListening() }
        invoke.resolve()
    }

    @Command
    fun speakChunk(invoke: Invoke) {
        val args = invoke.parseArgs(SpeakChunkArgs::class.java)
        val engine = tts
        if (engine == null || !ttsReady) {
            invoke.reject("TTS engine not ready")
            return
        }
        audioFocus.requestTts()
        engine.speak(args.text, TextToSpeech.QUEUE_ADD, null, System.currentTimeMillis().toString())
        invoke.resolve()
    }

    @Command
    fun stopSpeaking(invoke: Invoke) {
        tts?.stop()
        speaking = false
        audioFocus.releaseTts()
        invoke.resolve()
    }

    @Command
    fun ttsState(invoke: Invoke) {
        invoke.resolve(JSObject().put("speaking", speaking))
    }

    override fun onDestroy() {
        speechRecognizer?.destroy()
        tts?.shutdown()
        super.onDestroy()
    }

    private fun createRecognizer(): SpeechRecognizer {
        val onDeviceAvailable = Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            SpeechRecognizer.isOnDeviceRecognitionAvailable(activity)
        val recognizer = if (onDeviceAvailable) {
            SpeechRecognizer.createOnDeviceSpeechRecognizer(activity)
        } else {
            SpeechRecognizer.createSpeechRecognizer(activity)
        }
        recognizer.setRecognitionListener(
            object : RecognitionListener {
                override fun onReadyForSpeech(params: Bundle?) = Unit
                override fun onBeginningOfSpeech() = Unit
                override fun onRmsChanged(rmsdB: Float) = Unit
                override fun onBufferReceived(buffer: ByteArray?) = Unit

                override fun onEndOfSpeech() {
                    audioFocus.releaseCapture()
                }

                override fun onError(error: Int) {
                    audioFocus.releaseCapture()
                    trigger("voice-stt-error", JSObject().put("error", describeError(error)))
                }

                override fun onResults(results: Bundle?) {
                    audioFocus.releaseCapture()
                    val text = firstResult(results)
                    trigger("voice-stt-final", JSObject().put("text", text))
                }

                override fun onPartialResults(partialResults: Bundle?) {
                    trigger("voice-stt-partial", JSObject().put("text", firstResult(partialResults)))
                }

                override fun onEvent(eventType: Int, params: Bundle?) = Unit
            },
        )
        return recognizer
    }

    private fun firstResult(results: Bundle?): String =
        results?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)?.firstOrNull().orEmpty()

    private fun buildRecognizerIntent(): Intent =
        Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH).apply {
            putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
            putExtra(RecognizerIntent.EXTRA_LANGUAGE, "vi-VN")
            putExtra(RecognizerIntent.EXTRA_PARTIAL_RESULTS, true)
            // A hint only (researcher-02) — the device may still use network if no offline vi-VN
            // model is installed; network stays the primary, unconditional path regardless.
            putExtra(RecognizerIntent.EXTRA_PREFER_OFFLINE, false)
        }

    private fun describeError(error: Int): String = when (error) {
        SpeechRecognizer.ERROR_NETWORK -> "network"
        SpeechRecognizer.ERROR_NETWORK_TIMEOUT -> "network-timeout"
        SpeechRecognizer.ERROR_NO_MATCH -> "no-match"
        SpeechRecognizer.ERROR_SPEECH_TIMEOUT -> "speech-timeout"
        SpeechRecognizer.ERROR_INSUFFICIENT_PERMISSIONS -> "permission-denied"
        SpeechRecognizer.ERROR_AUDIO -> "audio"
        SpeechRecognizer.ERROR_RECOGNIZER_BUSY -> "recognizer-busy"
        else -> "unknown"
    }
}

@InvokeArg
class SpeakChunkArgs {
    lateinit var text: String
}
