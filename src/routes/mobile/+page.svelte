<script lang="ts">
  // Mobile Thin-Client plan phase 3 — the mobile app's entry route (M6 split: this route is
  // built alongside the desktop root route in one SvelteKit project; `npm run build:mobile`
  // + `src-tauri-mobile/tauri.conf.json`'s `windows[0].url: "mobile"` are what make a MOBILE
  // Tauri build actually open at THIS page instead of the desktop one).
  import { onMount } from 'svelte';
  import {
    mobileStatus,
    onConnectionState,
    onMobileKillState,
    mobileEnableKillSwitch,
    mobileUnpair,
    type MobileConnectionState,
  } from './mobile-tauri';
  import ConnectionBanner from './ConnectionBanner.svelte';
  import PairingScreen from './PairingScreen.svelte';
  import MobileChat from './MobileChat.svelte';

  let connState = $state<MobileConnectionState>({ paired: false, connected: false, reason: null });
  let killOn = $state(false);
  let enablingKill = $state(false);
  // Session-scoped calls (kill switch) need SOME session id; the chat view mints its own
  // per-turn session ids, but the kill switch is a device/session-independent safety control on
  // the wire per m1's session_id-carrying rule — a nil placeholder id satisfies the frame shape
  // without implying the toggle is scoped to any one conversation (mirrors the desktop's
  // `NIL_UUID` sentinel for non-session-scoped signals).
  const NIL_SESSION = '00000000-0000-0000-0000-000000000000';

  onMount(() => {
    let unlistenConn: (() => void) | undefined;
    let unlistenKill: (() => void) | undefined;

    mobileStatus()
      .then((s) => (connState = s))
      .catch(() => {});

    onConnectionState((s) => (connState = s)).then((fn) => (unlistenConn = fn));
    onMobileKillState((s) => (killOn = s.on)).then((fn) => (unlistenKill = fn));

    return () => {
      unlistenConn?.();
      unlistenKill?.();
    };
  });

  async function enableKillSwitch() {
    if (enablingKill || killOn) return;
    enablingKill = true;
    try {
      await mobileEnableKillSwitch(NIL_SESSION);
    } catch (e) {
      console.error('mobileEnableKillSwitch failed', e);
    } finally {
      enablingKill = false;
    }
  }

  async function unpair() {
    if (!confirm('Forget this pairing? You will need to scan the QR code again.')) return;
    try {
      await mobileUnpair();
      connState = { paired: false, connected: false, reason: null };
    } catch (e) {
      console.error('mobileUnpair failed', e);
    }
  }
</script>

<div class="app">
  <header>
    <span class="logo">Haily</span>
    {#if connState.paired}
      <button class="kill" class:on={killOn} onclick={enableKillSwitch} disabled={enablingKill || killOn}>
        {killOn ? '🛑 Writes off' : 'Stop all writes'}
      </button>
      <button class="unpair" onclick={unpair} title="Forget this pairing">⎋</button>
    {/if}
  </header>

  {#if connState.paired}
    <ConnectionBanner state={connState} />
  {/if}

  {#if connState.paired}
    <MobileChat />
  {:else}
    <PairingScreen />
  {/if}
</div>

<style>
  :global(*) { box-sizing: border-box; margin: 0; padding: 0; }
  :global(body) {
    background: #0f0f12;
    color: #e0dff5;
    font-family: system-ui, sans-serif;
    font-size: 14px;
    height: 100dvh;
    overflow: hidden;
  }
  .app { display: flex; flex-direction: column; height: 100dvh; }

  header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px 16px;
    border-bottom: 1px solid #1e1e2e;
    flex-shrink: 0;
  }
  .logo { font-weight: 700; font-size: 16px; color: #c084fc; }

  .kill {
    margin-left: auto;
    padding: 6px 12px;
    min-height: 32px;
    border-radius: 8px;
    border: 1px solid #3a2a5a;
    background: #1e1638;
    color: #eab308;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .kill.on { background: #3a1f2e; color: #f87171; border-color: #7f1d1d; }
  .kill:disabled { opacity: 0.6; cursor: default; }

  .unpair {
    width: 32px;
    height: 32px;
    border-radius: 8px;
    border: 1px solid #2e2e4a;
    background: transparent;
    color: #8a86ac;
    cursor: pointer;
  }
</style>
