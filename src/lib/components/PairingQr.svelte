<script lang="ts">
  // P2b — QR pairing screen + the OOB confirm-on-pair prompt (M4). Minting a code from this
  // panel is a casual button press (unlike `haily pair`'s terminal-access-IS-the-confirm
  // ceremony), so every redemption creates a pending confirm this same panel must separately
  // approve — see `haily_app::mobile_admin`'s module doc. There is no PUSH event for a newly
  // arrived pairing request (P2a exposes no such channel, and adding one would mean editing
  // P2a's server internals, out of this phase's ownership) — this panel polls for one instead
  // while mounted. The confirm gate itself is fully real end-to-end either way: a phone cannot
  // obtain a token until `mobileConfirmPair` resolves it.
  import QRCode from 'qrcode';
  import {
    mobilePairingQr,
    mobilePendingPairs,
    mobileConfirmPair,
    type PairingQr as PairingQrPayload,
    type PendingPair,
  } from '$lib/tauri';

  const POLL_INTERVAL_MS = 2000;

  let deviceNameHint = $state('');
  let qr = $state<PairingQrPayload | null>(null);
  let qrImage = $state('');
  let minting = $state(false);
  let mintError = $state('');
  let pending = $state<PendingPair[]>([]);
  let resolving = $state(false);

  $effect(() => {
    pollPending();
    const handle = setInterval(pollPending, POLL_INTERVAL_MS);
    return () => clearInterval(handle);
  });

  async function pollPending() {
    try {
      pending = await mobilePendingPairs();
    } catch {
      // Feature not compiled in, or a transient IPC hiccup — no pending prompt either way.
      pending = [];
    }
  }

  async function mint() {
    if (minting) return;
    minting = true;
    mintError = '';
    qr = null;
    try {
      qr = await mobilePairingQr(deviceNameHint.trim() || undefined);
      qrImage = await QRCode.toDataURL(JSON.stringify(qr), { margin: 1, width: 220 });
    } catch (e) {
      mintError = String(e);
    } finally {
      minting = false;
    }
  }

  async function decide(code: string, approve: boolean) {
    if (resolving) return;
    resolving = true;
    try {
      await mobileConfirmPair(code, approve);
    } finally {
      resolving = false;
      await pollPending();
    }
  }

  function expiresInLabel(expiresAt: string): string {
    const ms = new Date(expiresAt).getTime() - Date.now();
    if (ms <= 0) return 'expired — generate a new code';
    return `expires in ${Math.max(1, Math.round(ms / 1000))}s`;
  }
</script>

<div class="section">
  {#each pending as p (p.code)}
    <div class="block confirm-prompt">
      <span class="warning-title">📱 A device wants to pair</span>
      <span class="hint">{p.device_name} · code {p.code} — only approve this if you just scanned it yourself.</span>
      <div class="actions">
        <button class="deny" onclick={() => decide(p.code, false)} disabled={resolving}>Deny</button>
        <button class="approve" onclick={() => decide(p.code, true)} disabled={resolving}>Approve</button>
      </div>
    </div>
  {/each}

  <div class="block">
    <span class="switch-title">Add a device</span>
    <span class="hint">
      Scan this from the Haily mobile app to pair a new phone. Codes expire quickly, and every
      pairing needs your approval above before the phone gets access.
    </span>
    <input
      placeholder="Device name (optional)"
      autocomplete="off"
      bind:value={deviceNameHint}
      disabled={minting}
    />
    <button class="acp-btn" onclick={mint} disabled={minting}>
      {minting ? 'Generating…' : 'Generate pairing QR'}
    </button>
    {#if mintError}<div class="status-error">⚠️ {mintError}</div>{/if}
    {#if qr}
      <div class="qr-wrap">
        <img src={qrImage} alt="Pairing QR code" width="220" height="220" />
        <span class="hint">Code {qr.pairing_code} · {expiresInLabel(qr.expires_at)}</span>
        <!-- D7: the code above is the primary anti-photographed-QR discriminator (the OOB
             confirm prompt shows it too, matched against the same value the phone reports) —
             the fingerprint is shown here alongside the QR for a manual visual double-check
             against whatever the phone's own scan screen displays before it connects. -->
        <span class="hint fingerprint">Fingerprint: {qr.cert_fingerprint}</span>
      </div>
    {/if}
  </div>
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 16px; }
  .block { display: flex; flex-direction: column; gap: 8px; }
  .switch-title { font-size: 13px; color: #e0dff5; font-weight: 600; }
  .hint { font-size: 11px; color: #6b6b8a; line-height: 1.5; }

  input {
    padding: 6px 10px;
    border-radius: 7px;
    border: 1px solid #2e2e4a;
    background: #0f0f1e;
    color: #e0dff5;
    font-size: 12px;
  }

  .acp-btn {
    align-self: flex-start;
    padding: 6px 14px;
    min-height: 32px;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
    background: #16162a;
    color: #a09ac0;
    font-size: 12px;
    cursor: pointer;
  }
  .acp-btn:hover:not(:disabled) { border-color: #4b4b6a; color: #e0dff5; }
  .acp-btn:disabled { opacity: 0.5; cursor: default; }

  .qr-wrap {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 8px;
    padding: 12px;
    background: #0f0f1e;
    border: 1px solid #2e2e4a;
    border-radius: 10px;
  }
  .qr-wrap img { border-radius: 6px; background: #fff; padding: 8px; }
  .fingerprint { font-family: monospace; word-break: break-all; }

  .confirm-prompt {
    padding: 12px;
    background: #2a1f0f;
    border: 1px solid #7f5a1d;
    border-radius: 10px;
  }
  .warning-title { font-size: 12px; font-weight: 600; color: #fbbf24; }

  .actions { display: flex; gap: 8px; }
  .deny, .approve {
    padding: 6px 14px;
    border-radius: 7px;
    border: none;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .deny { background: #2a2a45; color: #ddd8f5; }
  .approve { background: #7c3aed; color: #fff; }
  .deny:disabled, .approve:disabled { opacity: 0.5; cursor: default; }

  .status-error {
    font-size: 11px;
    padding: 6px 10px;
    border-radius: 6px;
    background: #2a0f0f;
    color: #f87171;
    border: 1px solid #7f1d1d;
    word-break: break-word;
  }
</style>
