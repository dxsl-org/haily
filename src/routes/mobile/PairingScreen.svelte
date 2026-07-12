<script lang="ts">
  // QR scan (primary) + manual host/code entry (fallback — the `adb reverse` loopback dev
  // loop has no QR to scan, and typing avoids depending on a working camera for local testing,
  // M12). Redemption itself (`/pair` + OOB desktop confirm, M4) happens identically either way
  // via `mobilePair`.
  import { mobilePair, scanPairingQr, type PairingQrPayload } from './mobile-tauri';

  let { deviceName = $bindable('') }: { deviceName?: string } = $props();

  let mode = $state<'idle' | 'scanning' | 'manual' | 'waiting'>('idle');
  let error = $state('');

  let manualHost = $state('');
  let manualPort = $state('7443');
  let manualFingerprint = $state('');
  let manualCode = $state('');

  async function redeem(qr: PairingQrPayload) {
    mode = 'waiting';
    error = '';
    try {
      await mobilePair(qr, deviceName.trim() || 'Phone');
      // Success flips `paired` via the `mobile-connection-state` event the parent listens on;
      // nothing further to do here.
    } catch (e) {
      error = String(e);
      mode = 'idle';
    }
  }

  async function startScan() {
    mode = 'scanning';
    error = '';
    try {
      const qr = await scanPairingQr();
      await redeem(qr);
    } catch (e) {
      error = String(e);
      mode = 'idle';
    }
  }

  async function submitManual(e: Event) {
    e.preventDefault();
    const port = Number.parseInt(manualPort, 10);
    if (!manualHost.trim() || !manualFingerprint.trim() || !manualCode.trim() || Number.isNaN(port)) {
      error = 'Fill in host, port, fingerprint, and pairing code.';
      return;
    }
    await redeem({
      host: manualHost.trim(),
      port,
      cert_fingerprint: manualFingerprint.trim(),
      pairing_code: manualCode.trim(),
      expires_at: new Date(Date.now() + 60_000).toISOString(),
    });
  }
</script>

<div class="pairing">
  <h1>Pair with Haily</h1>
  <p class="hint">Scan the QR code shown in Haily's desktop app, then approve the request there.</p>

  <input placeholder="Device name (optional)" bind:value={deviceName} autocomplete="off" />

  {#if mode === 'waiting'}
    <p class="waiting">Waiting for approval on your computer…</p>
  {:else}
    <button class="primary" onclick={startScan} disabled={mode === 'scanning'}>
      {mode === 'scanning' ? 'Opening camera…' : '📷 Scan QR code'}
    </button>
    <button class="link" onclick={() => (mode = mode === 'manual' ? 'idle' : 'manual')}>
      {mode === 'manual' ? 'Hide manual entry' : 'Enter details manually (dev/no camera)'}
    </button>
  {/if}

  {#if mode === 'manual'}
    <form onsubmit={submitManual}>
      <input placeholder="Host (e.g. 127.0.0.1)" bind:value={manualHost} autocomplete="off" />
      <input placeholder="Port" bind:value={manualPort} inputmode="numeric" />
      <input placeholder="Cert fingerprint (sha256:…)" bind:value={manualFingerprint} autocomplete="off" />
      <input placeholder="Pairing code" bind:value={manualCode} autocomplete="off" />
      <button type="submit" class="primary">Pair</button>
    </form>
  {/if}

  {#if error}<div class="error">⚠️ {error}</div>{/if}
</div>

<style>
  .pairing {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 20px;
    max-width: 420px;
    margin: 0 auto;
  }
  h1 { font-size: 18px; color: #c084fc; }
  .hint { font-size: 12px; color: #8a86ac; line-height: 1.5; }
  .waiting { font-size: 13px; color: #eab308; }

  input {
    padding: 9px 12px;
    border-radius: 8px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #e0dff5;
    font-size: 13px;
  }

  form { display: flex; flex-direction: column; gap: 8px; }

  button {
    padding: 10px 14px;
    border-radius: 10px;
    border: none;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
  }
  .primary { background: #7c3aed; color: #fff; }
  .primary:disabled { opacity: 0.6; cursor: default; }
  .link { background: transparent; color: #a09ac0; text-decoration: underline; font-weight: 400; }

  .error {
    font-size: 12px;
    padding: 8px 10px;
    border-radius: 8px;
    background: #2a0f0f;
    color: #f87171;
    border: 1px solid #7f1d1d;
    word-break: break-word;
  }
</style>
