// Mobile-only Tauri IPC wrappers (Mobile Thin-Client plan phase 3). `send_message`,
// `approve_tool`, `haily-chunk`, `haily-proactive-cards` are DELIBERATELY re-used from
// `$lib/tauri.ts` unmodified — `src-tauri-mobile` registers commands/emits events under those
// SAME names so `ApprovalModal.svelte`/`ProactivePanel.svelte` work here without any change
// (the desktop-only commands those two components call resolve against WHICHEVER Tauri
// backend is actually running the webview, so pointing this app's own Rust layer at identical
// names is enough — see the phase's Deviation Log for why this reuse works instead of forking
// the components).
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

/** Sends a chat message with a CALLER-SUPPLIED session id (unlike the shared
 * `$lib/tauri.ts::sendMessage`, which mints one server-side) — lets `MobileChat.svelte`
 * pre-register its `sessionIndex` entry before this call resolves, closing a race where a
 * `haily-chunk` event for the new session could otherwise arrive over IPC before the command's
 * return value does. */
export async function mobileSendMessage(sessionId: string, message: string): Promise<void> {
  return invoke('mobile_send_message', { sessionId, message });
}

/** Mirrors `haily_types::PairingQr` (the QR payload) — decoded either from a camera scan or
 * typed in manually via the dev-loop fallback form. */
export interface PairingQrPayload {
  host: string;
  port: number;
  cert_fingerprint: string;
  pairing_code: string;
  expires_at: string;
}

/** The three distinguishable "why can't I talk to the desktop" reasons (m5) — the banner's
 * copy differs meaningfully between them (see `ConnectionBanner.svelte`). `null` means none of
 * these apply (either fully connected, or not yet paired at all). */
export type MobileDisconnectReason = 'unreachable' | 'auth_rejected' | 're_pair' | null;

export interface MobileConnectionState {
  paired: boolean;
  connected: boolean;
  reason: MobileDisconnectReason;
}

/** Subscribe to connection-state transitions — connect/disconnect/re-pair-needed. Emitted by
 * the Rust IPC bridge whenever `haily_mobile_client::ClientEvent` changes state. */
export async function onConnectionState(
  callback: (state: MobileConnectionState) => void,
): Promise<UnlistenFn> {
  return listen<MobileConnectionState>('mobile-connection-state', (e) => callback(e.payload));
}

export interface MobileKillState {
  on: boolean;
}

/** Subscribe to kill-switch state broadcasts (global across every channel, M15). */
export async function onMobileKillState(
  callback: (state: MobileKillState) => void,
): Promise<UnlistenFn> {
  return listen<MobileKillState>('mobile-kill-state', (e) => callback(e.payload));
}

/** Redeem a scanned/entered pairing QR: `POST /pair`, store the returned token in the OS-backed
 * Stronghold vault (m8), then start the WS client. Blocks until the desktop's out-of-band
 * confirm resolves it (or the code expires) — the caller should show a "waiting for approval on
 * your computer" state while this is pending. */
export async function mobilePair(qr: PairingQrPayload, deviceName: string): Promise<void> {
  return invoke('mobile_pair', { qr, deviceName });
}

/** Read the current connection/pairing status once (e.g. on app launch, before any event has
 * fired yet). */
export async function mobileStatus(): Promise<MobileConnectionState> {
  return invoke('mobile_status');
}

/** Clears the stored device token and disconnects — the phone forgets this pairing entirely. */
export async function mobileUnpair(): Promise<void> {
  return invoke('mobile_unpair');
}

/** ENABLE-ONLY from mobile (M1) — the Rust command itself rejects `on: false` before it ever
 * reaches the wire, mirroring the server's own enforcement so a compromised/patched frontend
 * still cannot disable safety remotely from two independent layers. */
export async function mobileEnableKillSwitch(sessionId: string): Promise<void> {
  return invoke('mobile_set_kill_switch', { sessionId, on: true });
}

/** One transcript entry, mirrors `haily_types::TranscriptEntry`. */
export interface MobileTranscriptEntry {
  role: string;
  content: string;
}

/** Mirrors `haily_types::SessionSnapshot` — the resume-window-exceeded recovery payload (M7). */
export interface MobileSessionSnapshot {
  session_id: string;
  transcript: MobileTranscriptEntry[];
  latest_run_status: string | null;
  depth: string;
}

/** Requests a full session resync. The Rust bridge does NOT call this automatically — it has no
 * bookkeeping of which session(s) are currently open (only this Svelte layer knows that). On
 * `resume-window-exceeded`/an epoch change (server restart, C4) the bridge instead emits
 * `mobile-resync-needed` (see `onResyncNeeded`); the caller (`MobileChat.svelte`) reacts to that
 * event by calling this function itself for whichever session is currently active. Also usable
 * directly for a manual "resync" action. */
export async function mobileFetchSession(sessionId: string): Promise<MobileSessionSnapshot> {
  return invoke('mobile_fetch_session', { sessionId });
}

/** Subscribe to the resync-needed signal (M7/C4) — fired when the server's epoch changed
 * (restart) or a resume-by-seq attempt reported `resume_window_exceeded`. The callback should
 * call `mobileFetchSession` for whatever session is currently open and replace its local
 * transcript view wholesale (§6.3 — never merge/append). */
export async function onResyncNeeded(callback: () => void): Promise<UnlistenFn> {
  return listen('mobile-resync-needed', () => callback());
}

/** Mirrors the `mobile-approval-denied` event payload — emitted when the user tapped Approve but
 * the on-device biometric check failed/was cancelled (M1). `ApprovalModal.svelte` (shared,
 * desktop-owned) closes its dialog either way with no notion of this outcome, so a sibling
 * listener surfaces the denial instead of letting the UI silently imply the action went through. */
export interface MobileApprovalDenied {
  approval_id: string;
  reason: string;
}

export async function onApprovalDenied(
  callback: (denial: MobileApprovalDenied) => void,
): Promise<UnlistenFn> {
  return listen<MobileApprovalDenied>('mobile-approval-denied', (e) => callback(e.payload));
}

/** Scans a QR code via the camera and parses it as a [`PairingQrPayload`]. Requests camera
 * permission first if not already granted. Throws if the scanned content isn't valid JSON
 * shaped like a pairing QR (e.g. a random unrelated QR code) — never silently proceeds with a
 * malformed payload. */
export async function scanPairingQr(): Promise<PairingQrPayload> {
  const { scan, Format, checkPermissions, requestPermissions } = await import(
    '@tauri-apps/plugin-barcode-scanner'
  );
  let permission = await checkPermissions();
  if (permission !== 'granted') {
    permission = await requestPermissions();
  }
  if (permission !== 'granted') {
    throw new Error('Camera permission is required to scan a pairing QR code');
  }
  const result = await scan({ windowed: true, formats: [Format.QRCode] });
  let parsed: unknown;
  try {
    parsed = JSON.parse(result.content);
  } catch {
    throw new Error('Scanned code is not a Haily pairing QR');
  }
  const qr = parsed as Partial<PairingQrPayload>;
  if (
    typeof qr.host !== 'string' ||
    typeof qr.port !== 'number' ||
    typeof qr.cert_fingerprint !== 'string' ||
    typeof qr.pairing_code !== 'string'
  ) {
    throw new Error('Scanned code is not a Haily pairing QR');
  }
  return qr as PairingQrPayload;
}
