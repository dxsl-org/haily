<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';

  let { prefs, save }: {
    prefs: Record<string, string>;
    save: (key: string, value: string) => Promise<void>;
  } = $props();

  let sub = $state<'local' | 'cloud'>('local');

  interface ModelEntry { name: string; path: string; format: string; }
  let localModels = $state<ModelEntry[]>([]);
  let loadingModels = $state(false);

  async function loadModels() {
    loadingModels = true;
    try {
      localModels = await invoke<ModelEntry[]>('list_local_models');
    } finally {
      loadingModels = false;
    }
  }

  onMount(loadModels);

  // Current selection (matched by path)
  const currentPath = () => prefs['llm.llama_model_path'] ?? '';

  let reloadError = $state('');
  let reloading = $state(false);
  let loadedProvider = $state('');

  async function selectModel(path: string) {
    const entry = localModels.find(m => m.path === path);
    if (!entry) return;
    reloadError = '';
    loadedProvider = '';
    reloading = true;
    try {
      await save('llm.llama_model_path', entry.path);
      await save('llm.llama_prompt_format', entry.format);
      const provider = await invoke<string>('reload_llm');
      // The router never errors on load — it silently falls back to "unconfigured".
      // Only "llama.cpp" means the GGUF actually loaded.
      if (provider === 'llama.cpp') {
        loadedProvider = provider;
      } else {
        reloadError = `Model không nạp được (provider: ${provider}). File GGUF có thể quá lớn so với RAM, hỏng, hoặc thiếu phần (-of-).`;
      }
    } catch (e) {
      reloadError = String(e);
    } finally {
      reloading = false;
    }
  }

  const p = (key: string, fallback = '') => prefs[key] ?? fallback;
</script>

<div class="subtabs">
  {#each (['local', 'cloud'] as const) as t}
    <button class="subtab" class:active={sub === t} onclick={() => sub = t}>
      {t === 'local' ? 'Local GGUF' : 'Cloud API'}
    </button>
  {/each}
</div>

{#if sub === 'local'}
  <div class="section">

    <label>Model GGUF
      {#if loadingModels}
        <div class="loading">Đang quét thư mục models…</div>
      {:else if localModels.length === 0}
        <div class="empty">
          Không tìm thấy file .gguf trong thư mục <code>models/</code>.<br>
          Đặt file GGUF vào <code>&lt;thư mục app&gt;/models/</code> rồi bấm làm mới.
          <button class="refresh-btn" onclick={loadModels}>↻ Làm mới</button>
        </div>
      {:else}
        <div class="model-row">
          <select
            value={currentPath()}
            onchange={e => selectModel(e.currentTarget.value)}
          >
            {#if !currentPath()}
              <option value="">— chọn model —</option>
            {/if}
            {#each localModels as m}
              <option value={m.path}>
                {m.name}  ({m.format === 'gemma4' ? 'Gemma4' : 'ChatML'})
              </option>
            {/each}
          </select>
          <button class="refresh-btn icon" onclick={loadModels} title="Làm mới danh sách">↻</button>
        </div>
      {/if}
    </label>

    {#if reloading}
      <div class="status-info">⏳ Đang tải model…</div>
    {:else if reloadError}
      <div class="status-error">⚠️ {reloadError}</div>
    {:else if loadedProvider}
      <div class="status-ok">✓ Model đã nạp ({loadedProvider})</div>
    {/if}

    <label>Context window (tokens)
      <input type="number" min="512" max="131072" step="512"
        value={p('llm.llama_n_ctx', '4096')}
        onblur={e => save('llm.llama_n_ctx', e.currentTarget.value)} />
    </label>

    <label>GPU layers
      <input type="number" min="0" max="999"
        value={p('llm.llama_n_gpu_layers', '0')}
        onblur={e => save('llm.llama_n_gpu_layers', e.currentTarget.value)} />
      <span class="hint">0 = CPU-only &nbsp;·&nbsp; 999 = full GPU offload</span>
    </label>

  </div>

{:else}
  <div class="section">
    <label>Base URL
      <input type="text" value={p('llm.cloud_base_url', 'https://api.openai.com')}
        onblur={e => save('llm.cloud_base_url', e.currentTarget.value)} />
    </label>
    <label>Model
      <input type="text" value={p('llm.cloud_model', 'gpt-4o-mini')}
        onblur={e => save('llm.cloud_model', e.currentTarget.value)} />
    </label>
    <label>API Key
      <input type="password" value={p('llm.cloud_api_key')}
        placeholder="sk-..."
        onblur={e => save('llm.cloud_api_key', e.currentTarget.value)} />
    </label>
  </div>
{/if}

<style>
  .subtabs {
    display: flex;
    gap: 4px;
    margin-bottom: 20px;
    background: #0f0f18;
    padding: 4px;
    border-radius: 10px;
  }
  .subtab {
    flex: 1;
    padding: 6px;
    border: none;
    border-radius: 7px;
    background: transparent;
    color: #6b6b8a;
    font-size: 12px;
    cursor: pointer;
    transition: all 0.15s;
  }
  .subtab.active { background: #1e1e35; color: #e0dff5; }
  .subtab:hover:not(.active) { color: #a09ac0; }

  .section { display: flex; flex-direction: column; gap: 18px; }

  label {
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-size: 12px;
    color: #8884aa;
  }

  input, select {
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
    color: #e0dff5;
    font: inherit;
    font-size: 13px;
    padding: 8px 10px;
    outline: none;
    transition: border-color 0.15s;
    width: 100%;
  }
  input:focus, select:focus { border-color: #7c3aed; }
  input::placeholder { color: #4a4a6a; }

  .model-row { display: flex; gap: 6px; align-items: center; }
  .model-row select { flex: 1; }

  .refresh-btn {
    flex-shrink: 0;
    padding: 6px 10px;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
    background: #16162a;
    color: #8884aa;
    font-size: 13px;
    cursor: pointer;
    transition: color 0.15s, border-color 0.15s;
    white-space: nowrap;
  }
  .refresh-btn.icon { width: 34px; font-size: 16px; padding: 6px 0; text-align: center; }
  .refresh-btn:hover { color: #c084fc; border-color: #4a3a7a; }

  .hint { font-size: 11px; color: #4a4a6a; }

  .status-ok, .status-error, .status-info {
    font-size: 11px;
    padding: 6px 10px;
    border-radius: 6px;
  }
  .status-ok    { background: #0f2a1a; color: #4ade80; border: 1px solid #166534; }
  .status-error { background: #2a0f0f; color: #f87171; border: 1px solid #7f1d1d; word-break: break-word; }
  .status-info  { background: #1a1a2a; color: #a0a0c0; border: 1px solid #2e2e4a; }

  .loading, .empty {
    font-size: 12px;
    color: #6b6b8a;
    line-height: 1.6;
    padding: 10px;
    background: #0f0f18;
    border-radius: 8px;
    border: 1px dashed #2e2e4a;
  }
  .empty code {
    color: #a09ac0;
    font-size: 11px;
    background: #1a1a30;
    padding: 1px 5px;
    border-radius: 4px;
  }
</style>
