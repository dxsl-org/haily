package io.haily.voice

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager

/**
 * Wraps Android's audio-focus API (researcher-02): capture (push-to-talk recording) requests
 * `AUDIOFOCUS_GAIN_TRANSIENT_EXCLUSIVE`, released the instant the recognizer session ends rather
 * than held for the app's whole lifetime — that both protects mic input quality during capture
 * AND lets other apps' audio resume immediately after. TTS playback requests the weaker
 * `AUDIOFOCUS_GAIN_TRANSIENT` so an incoming call/notification DUCKS it instead of hard-stopping;
 * [onTtsShouldPause] is the hook `HailyVoicePlugin` wires to its own `TextToSpeech.stop()` call
 * for a transient loss (a full `AUDIOFOCUS_LOSS` instead releases outright via [releaseTts]).
 */
class AudioFocus(context: Context) {
    private val audioManager = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager
    private var captureRequest: AudioFocusRequest? = null
    private var ttsRequest: AudioFocusRequest? = null

    /** Invoked on `AUDIOFOCUS_LOSS_TRANSIENT` while TTS focus is held — the plugin sets this to
     * its own pause logic; left null-safe so this class has no compile-time dependency on
     * `TextToSpeech`. */
    var onTtsShouldPause: (() -> Unit)? = null

    private fun speechAttributes(): AudioAttributes =
        AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_ASSISTANT)
            .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
            .build()

    fun requestCapture() {
        val request = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN_TRANSIENT_EXCLUSIVE)
            .setAudioAttributes(speechAttributes())
            .build()
        captureRequest = request
        audioManager.requestAudioFocus(request)
    }

    fun releaseCapture() {
        captureRequest?.let { audioManager.abandonAudioFocusRequest(it) }
        captureRequest = null
    }

    fun requestTts() {
        // Multi-sentence queues call this once per chunk; focus is already held, and building a
        // fresh request each time would overwrite `ttsRequest` WITHOUT abandoning the prior one —
        // leaking focus requests the final `releaseTts` can no longer abandon.
        if (ttsRequest != null) return
        val request = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN_TRANSIENT)
            .setAudioAttributes(speechAttributes())
            .setOnAudioFocusChangeListener { change ->
                when (change) {
                    AudioManager.AUDIOFOCUS_LOSS_TRANSIENT -> onTtsShouldPause?.invoke()
                    AudioManager.AUDIOFOCUS_LOSS -> releaseTts()
                    else -> Unit
                }
            }
            .build()
        ttsRequest = request
        audioManager.requestAudioFocus(request)
    }

    fun releaseTts() {
        ttsRequest?.let { audioManager.abandonAudioFocusRequest(it) }
        ttsRequest = null
    }
}
