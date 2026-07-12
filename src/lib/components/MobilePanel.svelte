<script lang="ts">
  // P2b — the "Mobile" block for `ChannelsPanel.svelte`: status banners (M2/M11 tailnet-absent/
  // degraded, "Also apply" Tailscale-prerequisite), the pairing entry point, the devices list,
  // and cert lifecycle (m5). Extracted into its own component to keep `ChannelsPanel.svelte`
  // under the file-size convention — this is purely a size split, not a reuse boundary.
  import { mobileServerStatus, mobileRegenerateCert, type MobileStatus } from '$lib/tauri';
  import PairingQr from './PairingQr.svelte';
  import DevicesPanel from './DevicesPanel.svelte';

  // `mobileServerStatus` rejects with a generic "command not found" error on a build compiled
  // without the `mobile-server` Rust feature — treated the same as "no status available" rather
  // than surfaced as an error, since that build simply doesn't have this channel at all.
  let status = $state<MobileStatus | null>(null);
  let showPairing = $state(false);
  let regenerating = $state(false);
  let regenError = $state('');
  let regenDone = $state(false);

  $effect(() => {
    load();
  });

  async function load() {
    try {
      status = await mobileServerStatus();
    } catch {
      status = null;
    }
  }

  async function regenerateCert() {
    if (regenerating) return;
    // m5/review finding 3: regenerating is IDENTITY ROTATION, not access revocation — every
    // already-paired phone's device row stays intact and keeps working over Tailscale/this-
    // computer; only its pinned Wi-Fi-direct fingerprint goes stale until it re-pairs. This
    // does NOT lock any device out — use Revoke below for that.
    if (
      !confirm(
        "This rotates the desktop's certificate identity. Every already-paired phone will need " +
          "to re-pair the next time it connects over Wi-Fi direct — it does NOT revoke or lock " +
          'out any device (their entries stay below; use Revoke for that). Continue?',
      )
    ) {
      return;
    }
    regenerating = true;
    regenError = '';
    regenDone = false;
    try {
      await mobileRegenerateCert();
      regenDone = true;
    } catch (e) {
      regenError = String(e);
    } finally {
      regenerating = false;
    }
  }
</script>

<!--
  Review finding 1: `status` is `null` in exactly one case — the Rust `mobile-server` feature
  isn't compiled in (a compiled-in build's `mobile_server_status` always returns a value, even
  disabled). Gating the WHOLE block on it (not just the banners) avoids rendering a "Pair a
  phone" button, a `DevicesPanel` that immediately error-toasts on a missing command, and a
  "Regenerate certificate" button that can never work, on a default GUI build.
-->
{#if status}
  <div class="block">
    <span class="switch-title">Mobile</span>
    {#if !status.tailnet_present}
      <div class="status-warning">
        ⚠️ Tailscale isn't detected. Pairing a phone needs a Tailscale tunnel running on this
        computer (or an explicit Wi-Fi-direct opt-in) — install/start Tailscale first.
      </div>
    {:else if status.enabled && !status.running}
      <div class="status-warning">
        ⚠️ Mobile server is turned on but doesn't seem to be running — its port may already be
        in use by something else.
      </div>
    {/if}
    <span class="hint">
      {status.enabled ? 'On' : 'Off, port ' + status.port}{status.lan_opt_in ? ' · Wi-Fi direct allowed' : ''}
    </span>
    <button class="acp-btn" onclick={() => (showPairing = !showPairing)}>
      {showPairing ? 'Hide device pairing' : 'Pair a phone'}
    </button>
    {#if showPairing}<PairingQr />{/if}
    <DevicesPanel />
    <button class="acp-btn" onclick={regenerateCert} disabled={regenerating}>
      {regenerating ? 'Regenerating…' : 'Regenerate certificate'}
    </button>
    <span class="hint">
      Rotates the desktop's identity — already-paired phones re-pair on their next Wi-Fi-direct
      connection. It does not revoke access; use Revoke above to lock a device out.
    </span>
    {#if regenDone}
      <span class="hint success">New certificate generated — phones paired over Wi-Fi direct will need to re-pair.</span>
    {/if}
    {#if regenError}<div class="status-error">⚠️ {regenError}</div>{/if}
  </div>
{/if}

<style>
  .block { display: flex; flex-direction: column; gap: 8px; }
  .switch-title { font-size: 13px; color: #e0dff5; font-weight: 600; }
  .hint { font-size: 11px; color: #6b6b8a; line-height: 1.5; }
  .hint.success { color: #4ade80; }

  .status-warning {
    padding: 10px;
    background: #2a1f0f;
    border: 1px solid #7f5a1d;
    border-radius: 8px;
    font-size: 11px;
    color: #fbbf24;
    line-height: 1.5;
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
