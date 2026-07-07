<script lang="ts">
  // Per-connector card for `ConnectorConfig.svelte` (Phase 7): credential set/rotate,
  // enable/disable, and a re-approval banner. The credential input is write-only — its
  // value is never pre-filled, never echoed back, and cleared immediately after a
  // successful save (see `setConnectorCredential`'s contract: it lands in the OS keyring,
  // never SQLite).
  import { setConnectorCredential, setConnectorStatus, acknowledgeConnectorVersion } from '$lib/tauri';
  import type { ConnectorSummary } from '$lib/tauri';

  let { connector, onChanged }: { connector: ConnectorSummary; onChanged: () => void } = $props();

  let secret = $state('');
  let savingCred = $state(false);
  let credError = $state('');
  let credSaved = $state(false);

  let togglingStatus = $state(false);
  let statusError = $state('');
  let acking = $state(false);

  const isActive = () => connector.status === 'active';

  async function saveCredential() {
    const credRef = connector.cred_ref;
    if (!credRef || savingCred || !secret.trim()) return;
    savingCred = true;
    credError = '';
    credSaved = false;
    try {
      await setConnectorCredential(credRef, secret);
      secret = '';
      credSaved = true;
    } catch (e) {
      credError = String(e);
    } finally {
      savingCred = false;
    }
  }

  async function toggleStatus() {
    if (togglingStatus) return;
    togglingStatus = true;
    statusError = '';
    try {
      await setConnectorStatus(connector.id, isActive() ? 'disabled' : 'active');
      onChanged();
    } catch (e) {
      statusError = String(e);
    } finally {
      togglingStatus = false;
    }
  }

  async function acknowledge() {
    const reapproval = connector.reapproval;
    if (!reapproval || acking) return;
    acking = true;
    try {
      await acknowledgeConnectorVersion(connector.connector_name, reapproval.live_version);
      onChanged();
    } finally {
      acking = false;
    }
  }
</script>

<div class="card">
  <div class="head">
    <div class="name-block">
      <span class="name">{connector.connector_name}</span>
      <span class="badge">{connector.risk_tier}</span>
      <span class="badge" class:on={isActive()} class:off={!isActive()}>
        {isActive() ? 'Active' : 'Disabled'}
      </span>
    </div>
    <span class="meta">v{connector.version} · {connector.base_url_host}</span>
  </div>

  {#if connector.reapproval}
    <div class="block reapproval">
      <span class="warning-title">⚠️ This connector's setup changed</span>
      <span class="hint">
        Version {connector.reapproval.approved_version} → {connector.reapproval.live_version}.
        {#if connector.reapproval.diff.base_url}
          Login destination changed: {connector.reapproval.diff.base_url[0]} →
          {connector.reapproval.diff.base_url[1]}.
        {/if}
        {#if connector.reapproval.diff.added_ops.length}
          New actions added: {connector.reapproval.diff.added_ops.join(', ')}.
        {/if}
        {#if connector.reapproval.diff.removed_ops.length}
          Actions removed: {connector.reapproval.diff.removed_ops.join(', ')}.
        {/if}
      </span>
      <button class="dismiss-btn" onclick={acknowledge} disabled={acking}>
        {acking ? 'Saving…' : "I've reviewed this"}
      </button>
    </div>
  {/if}

  {#if connector.cred_ref}
    <div class="block">
      <span class="switch-title">Login credential</span>
      <div class="cred-row">
        <input
          type="password"
          placeholder="Paste new credential (never shown again)"
          autocomplete="off"
          bind:value={secret}
          disabled={savingCred}
        />
        <button class="undo-btn" onclick={saveCredential} disabled={savingCred || !secret.trim()}>
          {savingCred ? 'Saving…' : 'Save'}
        </button>
      </div>
      {#if credSaved}<span class="hint success">Saved to secure storage.</span>{/if}
      {#if credError}<div class="status-error">⚠️ {credError}</div>{/if}
    </div>
  {/if}

  <div class="block switch-row">
    <div class="switch-copy">
      <span class="switch-title">{isActive() ? 'Turn off' : 'Turn on'}</span>
      <span class="hint">Takes effect the next time Haily restarts.</span>
    </div>
    <button
      class="switch"
      class:on={isActive()}
      role="switch"
      aria-checked={isActive()}
      aria-label={isActive() ? 'Turn off connector' : 'Turn on connector'}
      disabled={togglingStatus}
      onclick={toggleStatus}
    >
      <span class="knob"></span>
    </button>
  </div>
  {#if statusError}<div class="status-error">⚠️ {statusError}</div>{/if}
</div>

<style>
  .card {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 14px;
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 10px;
  }
  .head { display: flex; flex-direction: column; gap: 4px; }
  .name-block { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  .name { font-size: 13px; font-weight: 600; color: #e0dff5; }
  .meta { font-size: 11px; color: #6b6b8a; }

  .badge {
    font-size: 10px;
    padding: 2px 8px;
    border-radius: 999px;
    background: #1e1e35;
    border: 1px solid #2e2e4a;
    color: #a09ac0;
  }
  .badge.on { color: #4ade80; border-color: #166534; }
  .badge.off { color: #6b6b8a; }

  .block { display: flex; flex-direction: column; gap: 8px; }
  .switch-title { font-size: 12px; color: #e0dff5; font-weight: 600; }
  .hint { font-size: 11px; color: #6b6b8a; line-height: 1.5; }
  .hint.success { color: #4ade80; }

  .reapproval {
    padding: 10px;
    background: #2a1f0f;
    border: 1px solid #7f5a1d;
    border-radius: 8px;
  }
  .warning-title { font-size: 11px; font-weight: 600; color: #fbbf24; }

  .cred-row { display: flex; gap: 8px; }
  .cred-row input {
    flex: 1;
    padding: 6px 10px;
    border-radius: 7px;
    border: 1px solid #2e2e4a;
    background: #0f0f1e;
    color: #e0dff5;
    font-size: 12px;
  }

  .switch-row { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
  .switch-copy { display: flex; flex-direction: column; gap: 2px; }

  .dismiss-btn, .undo-btn {
    align-self: flex-start;
    padding: 5px 12px;
    border: 1px solid #7f5a1d;
    border-radius: 7px;
    background: #16162a;
    color: #fbbf24;
    font-size: 11px;
    cursor: pointer;
    transition: border-color 0.15s, background 0.15s;
  }
  .undo-btn { border-color: #2e2e4a; color: #c084fc; }
  .dismiss-btn:hover:not(:disabled) { border-color: #fbbf24; background: #1e1e35; }
  .undo-btn:hover:not(:disabled) { border-color: #7c3aed; background: #1e1e35; }
  .dismiss-btn:disabled, .undo-btn:disabled { opacity: 0.5; cursor: default; }

  .switch {
    flex-shrink: 0;
    width: 42px;
    height: 24px;
    border-radius: 999px;
    border: 1px solid #2e2e4a;
    background: #0f0f1e;
    cursor: pointer;
    position: relative;
    transition: background 0.15s, border-color 0.15s;
  }
  .switch:disabled { opacity: 0.5; cursor: default; }
  .switch .knob {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: #6b6b8a;
    transition: transform 0.15s, background 0.15s;
  }
  .switch.on { background: #4c1d95; border-color: #7c3aed; }
  .switch.on .knob { transform: translateX(18px); background: #e0dff5; }

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
