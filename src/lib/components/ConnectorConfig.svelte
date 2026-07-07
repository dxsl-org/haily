<script lang="ts">
  // Phase 7 (Connector config UI): human-only surface over installed connector manifests.
  // Manifest authoring (insert_version) stays out of scope — this only lists what is
  // already installed and lets a human set/rotate a credential or flip status. Split into
  // a list shell (this file) + `ConnectorRow.svelte` (per-connector detail/actions) to keep
  // both under the 200-line file-size rule, mirroring `JournalBrowser`/`JournalEntryRow`.
  import { listConnectors, type ConnectorSummary } from '$lib/tauri';
  import ConnectorRow from './ConnectorRow.svelte';

  let connectors = $state<ConnectorSummary[]>([]);
  let loading = $state(true);
  let error = $state('');

  $effect(() => {
    load();
  });

  async function load() {
    loading = true;
    error = '';
    try {
      connectors = await listConnectors();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }
</script>

<div class="section">
  <p class="hint">
    Connectors are installed and approved outside Haily (by whoever set this up). Here you
    can set or rotate a connector's login, turn it on or off, and review any changes to what
    it's allowed to do.
  </p>

  {#if loading}
    <div class="spinner">Đang tải…</div>
  {:else if error}
    <div class="status-error">⚠️ {error}</div>
  {:else if connectors.length === 0}
    <div class="hint">No connectors installed yet.</div>
  {:else}
    {#each connectors as connector (connector.id)}
      <ConnectorRow {connector} onChanged={load} />
    {/each}
  {/if}
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 16px; }
  .hint { font-size: 11px; color: #6b6b8a; line-height: 1.5; }
  .spinner { color: #6b6b8a; font-size: 13px; text-align: center; padding: 40px 0; }
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
