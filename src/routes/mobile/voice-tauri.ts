// Mobile-only Tauri IPC wrappers for the voice plugin (Mobile Thin-Client plan phase 4). All
// press/release/speak actions go through `src-tauri-mobile`'s OWN app-level commands (`voice_*`
// in `voice.rs`), never `tauri-plugin-haily-voice`'s commands directly — the Rust layer enforces
// the m4 press → stop-TTS + pause-chunker-feed ordering there, so this file has nothing to
// sequence, only to invoke/listen.
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

/** Fired repeatedly while the recognizer is still listening — live "what it's hearing so far"
 * preview text, never sent as a message on its own. */
export interface SttPartialPayload {
  text: string;
}

/** The recognizer's finalized transcript for this press/release cycle. */
export interface SttFinalPayload {
  text: string;
}

export interface SttErrorPayload {
  error: string;
}

export async function voicePttStart(): Promise<void> {
  return invoke('voice_ptt_start');
}

export async function voicePttStop(): Promise<void> {
  return invoke('voice_ptt_stop');
}

/** Resumes the chunker feed AND sends `text` as a normal chat message under `sessionId` — the
 * caller must pre-register its own `sessionIndex` entry for `sessionId` BEFORE calling this, the
 * same race-avoidance rule `mobileSendMessage` documents. */
export async function voiceSendTranscript(sessionId: string, text: string): Promise<void> {
  return invoke('voice_send_transcript', { sessionId, text });
}

/** The recognizer ended with no usable transcript (error/no-match/cancel) — resumes the chunker
 * feed only; nothing was sent. */
export async function voiceSttCancelled(): Promise<void> {
  return invoke('voice_stt_cancelled');
}

export async function voicePttIsListening(): Promise<boolean> {
  return invoke('voice_ptt_is_listening');
}

export async function voiceSetTtsEnabled(on: boolean): Promise<void> {
  return invoke('voice_set_tts_enabled', { on });
}

export interface VoiceTtsState {
  speaking: boolean;
}

export async function voiceTtsState(): Promise<VoiceTtsState> {
  return invoke('voice_tts_state');
}

/** Android's tri-state runtime permission result for `RECORD_AUDIO` — `"granted" | "denied" |
 * "prompt"`. On a host/dev-preview build (no plugin) this always resolves to `"denied"`, never a
 * fabricated `"granted"` (see `voice.rs`'s host stub). */
export interface VoicePermissionStatus {
  microphone: 'granted' | 'denied' | 'prompt';
}

export async function voiceCheckPermissions(): Promise<VoicePermissionStatus> {
  return invoke('voice_check_permissions');
}

export async function voiceRequestPermissions(): Promise<VoicePermissionStatus> {
  return invoke('voice_request_permissions');
}

export async function onSttPartial(
  callback: (payload: SttPartialPayload) => void,
): Promise<UnlistenFn> {
  return listen<SttPartialPayload>('voice-stt-partial', (e) => callback(e.payload));
}

export async function onSttFinal(
  callback: (payload: SttFinalPayload) => void,
): Promise<UnlistenFn> {
  return listen<SttFinalPayload>('voice-stt-final', (e) => callback(e.payload));
}

export async function onSttError(
  callback: (payload: SttErrorPayload) => void,
): Promise<UnlistenFn> {
  return listen<SttErrorPayload>('voice-stt-error', (e) => callback(e.payload));
}

/** Fired once per queued sentence finishing playback — not needed for m4 (the gate resumes off
 * the STT lifecycle, not TTS), but useful for a UI "speaking…" indicator. */
export async function onTtsDone(callback: () => void): Promise<UnlistenFn> {
  return listen('voice-tts-done', () => callback());
}
