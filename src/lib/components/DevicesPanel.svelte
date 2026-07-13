<script lang="ts">
  // P2b — paired-device list + revoke, for `ChannelsPanel.svelte`. Mirrors
  // `ConnectorConfig.svelte`'s list-shell shape (load-on-mount, per-row action, reload after).
  import { mobileListDevices, mobileRevokeDevice, type MobileDevice } from '$lib/tauri';

  let devices = $state<MobileDevice[]>([]);
  let loading = $state(true);
  let error = $state('');
  let revokingId = $state('');

  $effect(() => {
    load();
  });

  async function load() {
    loading = true;
    error = '';
    try {
      devices = await mobileListDevices();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function revoke(deviceId: string) {
    if (revokingId) return;
    revokingId = deviceId;
    try {
      await mobileRevokeDevice(deviceId);
      await load();
    } catch (e) {
      error = String(e);
    } finally {
      revokingId = '';
    }
  }

  function lastSeenLabel(d: MobileDevice): string {
    return d.last_seen_at ? new Date(d.last_seen_at).toLocaleString() : 'never connected yet';
  }
</script>

<div class="section">
  <span class="switch-title">Paired devices</span>
  {#if loading}
    <div class="hint">Loading…</div>
  {:else if error}
    <div class="status-error">⚠️ {error}</div>
  {:else if devices.length === 0}
    <div class="hint">No devices paired yet.</div>
  {:else}
    {#each devices as d (d.device_id)}
      <div class="device-row">
        <div class="device-info">
          <span class="device-name">{d.device_name}</span>
          <span class="hint">Last seen: {lastSeenLabel(d)}</span>
        </div>
        <button
          class="revoke-btn"
          onclick={() => revoke(d.device_id)}
          disabled={revokingId === d.device_id}
        >
          {revokingId === d.device_id ? 'Revoking…' : 'Revoke'}
        </button>
      </div>
    {/each}
  {/if}
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 10px; }
  .switch-title { font-size: 13px; color: #e0dff5; font-weight: 600; }
  .hint { font-size: 11px; color: #6b6b8a; line-height: 1.5; }

  .device-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 10px 12px;
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
  }
  .device-info { display: flex; flex-direction: column; gap: 2px; }
  .device-name { font-size: 12px; font-weight: 600; color: #e0dff5; }

  .revoke-btn {
    flex-shrink: 0;
    padding: 5px 12px;
    border: 1px solid #7f1d1d;
    border-radius: 7px;
    background: #16162a;
    color: #f87171;
    font-size: 11px;
    cursor: pointer;
    transition: border-color 0.15s, background 0.15s;
  }
  .revoke-btn:hover:not(:disabled) { background: #2a0f0f; }
  .revoke-btn:disabled { opacity: 0.5; cursor: default; }

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
