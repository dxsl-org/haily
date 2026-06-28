<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import ModelTab from './settings/ModelTab.svelte';
  import PersonaTab from './settings/PersonaTab.svelte';

  let { open = $bindable(false) } = $props();

  type Tab = 'model' | 'persona' | 'other';
  let tab = $state<Tab>('model');
  let prefs = $state<Record<string, string>>({});
  let loading = $state(false);

  $effect(() => {
    if (open) load();
  });

  async function load() {
    loading = true;
    try {
      const raw = await invoke<Record<string, unknown>>('get_preferences');
      // Values come as JSON strings — unwrap them.
      prefs = Object.fromEntries(
        Object.entries(raw).map(([k, v]) =>
          [k, typeof v === 'string' ? v : String(v)]
        )
      );
    } finally {
      loading = false;
    }
  }

  async function save(key: string, value: string) {
    prefs[key] = value;
    await invoke('set_preference', { key, value }).catch(() => {});
  }

  const tabs: { id: Tab; label: string }[] = [
    { id: 'model',   label: 'Model LLM' },
    { id: 'persona', label: 'Persona' },
    { id: 'other',   label: 'Khác' },
  ];
</script>

{#if open}
  <!-- Backdrop -->
  <div class="backdrop" onclick={() => open = false} role="presentation"></div>

  <!-- Drawer -->
  <div class="drawer" role="dialog" aria-label="Cài đặt" aria-modal="true">
    <header>
      <span class="title">Cài đặt</span>
      <button class="close" onclick={() => open = false} aria-label="Đóng">✕</button>
    </header>

    <!-- Tab bar -->
    <nav class="tabs">
      {#each tabs as t}
        <button
          class="tab"
          class:active={tab === t.id}
          onclick={() => tab = t.id}
        >{t.label}</button>
      {/each}
    </nav>

    <!-- Content -->
    <div class="content">
      {#if loading}
        <div class="spinner">Đang tải…</div>
      {:else if tab === 'model'}
        <ModelTab {prefs} {save} />
      {:else if tab === 'persona'}
        <PersonaTab {prefs} {save} />
      {:else}
        <div class="placeholder">
          <span class="ph-icon">🔧</span>
          <p>Sẽ bổ sung thêm trong các phiên bản tới.</p>
        </div>
      {/if}
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.55);
    z-index: 10;
  }

  div.drawer {
    position: fixed;
    top: 0;
    right: 0;
    bottom: 0;
    width: 340px;
    background: #111120;
    border-left: 1px solid #1e1e2e;
    z-index: 11;
    display: flex;
    flex-direction: column;
    animation: slide-in 0.18s ease-out;
  }

  @keyframes slide-in {
    from { transform: translateX(100%); opacity: 0; }
    to   { transform: translateX(0);    opacity: 1; }
  }

  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 14px 16px;
    border-bottom: 1px solid #1e1e2e;
    flex-shrink: 0;
  }

  .title { font-weight: 600; font-size: 14px; color: #e0dff5; }

  .close {
    width: 28px;
    height: 28px;
    border: none;
    border-radius: 7px;
    background: transparent;
    color: #6b6b8a;
    font-size: 14px;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: background 0.15s, color 0.15s;
  }
  .close:hover { background: #1e1e35; color: #e0dff5; }

  .tabs {
    display: flex;
    border-bottom: 1px solid #1e1e2e;
    flex-shrink: 0;
  }

  .tab {
    flex: 1;
    padding: 10px 8px;
    border: none;
    border-bottom: 2px solid transparent;
    background: transparent;
    color: #6b6b8a;
    font: inherit;
    font-size: 12px;
    cursor: pointer;
    transition: color 0.15s, border-color 0.15s;
    margin-bottom: -1px;
  }
  .tab:hover { color: #a09ac0; }
  .tab.active { color: #c084fc; border-bottom-color: #7c3aed; }

  .content {
    flex: 1;
    overflow-y: auto;
    padding: 20px 16px;
    scrollbar-width: thin;
    scrollbar-color: #2e2e45 transparent;
  }

  .spinner { color: #6b6b8a; font-size: 13px; text-align: center; padding: 40px 0; }

  .placeholder {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
    padding: 48px 0;
    color: #6b6b8a;
    font-size: 13px;
    text-align: center;
  }
  .ph-icon { font-size: 28px; }
</style>
