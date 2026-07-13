<script lang="ts">
  // Push-to-talk mic button + spoken-replies toggle (Mobile Thin-Client plan phase 4). Owns ONLY
  // the recording UI/permission flow — it does NOT mint session ids or touch the chat transcript;
  // `onTranscript` hands a finalized, non-empty transcript up to `MobileChat.svelte`, which owns
  // that bookkeeping (mirrors the typed-input send button's own session-id-first-then-send
  // pattern). The m4 press→stop-TTS+pause-chunker ordering is enforced entirely in Rust
  // (`voice.rs`); this component just calls `voicePttStart`/`voicePttStop` at the right DOM
  // events and reacts to whatever the recognizer reports back.
  import { onMount } from 'svelte';
  import {
    voicePttStart,
    voicePttStop,
    voiceSttCancelled,
    voiceSetTtsEnabled,
    voiceCheckPermissions,
    voiceRequestPermissions,
    onSttPartial,
    onSttFinal,
    onSttError,
  } from '../../../routes/mobile/voice-tauri';

  let {
    disabled = false,
    onTranscript,
    onError,
  }: {
    disabled?: boolean;
    onTranscript: (text: string) => void;
    onError?: (message: string) => void;
  } = $props();

  let recording = $state(false);
  let partial = $state('');
  let ttsOn = $state(false);
  let permissionDenied = $state(false);

  function vibrate(ms: number) {
    if (typeof navigator !== 'undefined' && 'vibrate' in navigator) {
      navigator.vibrate(ms);
    }
  }

  onMount(() => {
    const unlistenPartial = onSttPartial(({ text }) => {
      partial = text;
    });
    // `recording` only clears here (final/error), not on pointer-release — the recognizer keeps
    // processing a short tail after `stopStt`, and the indicator should stay visible through that
    // gap rather than snapping off the instant the finger lifts.
    const unlistenFinal = onSttFinal(({ text }) => {
      recording = false;
      partial = '';
      const trimmed = text.trim();
      if (!trimmed) {
        voiceSttCancelled().catch((e) => console.error('voiceSttCancelled failed', e));
        return;
      }
      onTranscript(trimmed);
    });
    const unlistenError = onSttError(({ error }) => {
      recording = false;
      partial = '';
      voiceSttCancelled().catch((e) => console.error('voiceSttCancelled failed', e));
      // "Heard nothing" outcomes (released without speaking, long silence) are benign — the gate
      // still resumes above; only real failures deserve an error banner.
      if (error !== 'no-match' && error !== 'speech-timeout') {
        onError?.(`Voice input failed: ${error}`);
      }
    });
    return () => {
      unlistenPartial.then((fn) => fn());
      unlistenFinal.then((fn) => fn());
      unlistenError.then((fn) => fn());
      // Unmounting mid-recording (navigation/background) would otherwise leave the Rust m4 gate
      // stuck in Listening — chunker feed paused, TTS silent — because the final/error event
      // that normally resumes it has no listener left. Stop the recognizer and resume the gate.
      if (recording) {
        voicePttStop().catch((e) => console.error('voicePttStop on unmount failed', e));
        voiceSttCancelled().catch((e) => console.error('voiceSttCancelled on unmount failed', e));
      }
    };
  });

  async function ensureMicPermission(): Promise<boolean> {
    const status = await voiceCheckPermissions();
    if (status.microphone === 'granted') return true;
    const requested = await voiceRequestPermissions();
    if (requested.microphone === 'granted') return true;
    permissionDenied = true;
    onError?.('Microphone permission is required for voice input');
    return false;
  }

  // True while the finger is physically down — distinct from `recording` (which only turns on
  // after the permission check). Guards the first-use race: a pointerup during the OS permission
  // prompt must abort the pending start, not leave the recognizer running with the finger up.
  let pointerHeld = false;

  async function press() {
    if (disabled || recording) return;
    pointerHeld = true;
    if (!(await ensureMicPermission())) return;
    if (!pointerHeld) return; // finger lifted while the OS permission prompt was up
    permissionDenied = false;
    recording = true;
    vibrate(30);
    try {
      await voicePttStart();
    } catch (e) {
      recording = false;
      onError?.(`Could not start voice input: ${e}`);
    }
  }

  async function release() {
    pointerHeld = false;
    if (!recording) return;
    vibrate(15);
    try {
      await voicePttStop();
    } catch (e) {
      console.error('voicePttStop failed', e);
    }
  }

  async function toggleTts() {
    ttsOn = !ttsOn;
    try {
      await voiceSetTtsEnabled(ttsOn);
    } catch (e) {
      console.error('voiceSetTtsEnabled failed', e);
    }
  }
</script>

<div class="voice-bar">
  <button
    class="tts-toggle"
    class:on={ttsOn}
    onclick={toggleTts}
    title={ttsOn ? 'Voice replies on' : 'Voice replies off'}
    aria-label="Toggle spoken replies"
  >
    {ttsOn ? '🔊' : '🔇'}
  </button>

  <button
    class="ptt"
    class:recording
    {disabled}
    onpointerdown={press}
    onpointerup={release}
    onpointerleave={release}
    onpointercancel={release}
    aria-label="Hold to record a voice message"
  >
    {recording ? '●' : '🎙'}
  </button>

  {#if recording}
    <span class="preview" role="status">{partial || 'Listening…'}</span>
  {/if}
  {#if permissionDenied}
    <span class="denied" role="alert">Mic permission denied</span>
  {/if}
</div>

<style>
  .voice-bar {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  button {
    border: none;
    border-radius: 10px;
    cursor: pointer;
  }
  .tts-toggle {
    width: 36px;
    height: 36px;
    background: #16162a;
    color: #8884aa;
    font-size: 16px;
  }
  .tts-toggle.on {
    background: #2a1f4a;
    color: #c084fc;
  }
  .ptt {
    width: 40px;
    height: 40px;
    background: #7c3aed;
    color: #fff;
    font-size: 18px;
    touch-action: none;
  }
  .ptt.recording {
    background: #dc2626;
    animation: pulse 1s ease-in-out infinite;
  }
  .ptt:disabled {
    opacity: 0.4;
    cursor: default;
  }
  @keyframes pulse {
    50% {
      opacity: 0.6;
    }
  }
  .preview {
    font-size: 12px;
    color: #8884aa;
    max-width: 140px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .denied {
    font-size: 11px;
    color: #f87171;
  }
</style>
