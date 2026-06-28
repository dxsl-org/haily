<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';

  let { prefs, save }: {
    prefs: Record<string, string>;
    save: (key: string, value: string) => Promise<void>;
  } = $props();

  let sub = $state<'ollama' | 'local' | 'cloud'>('ollama');
  let ollamaModels = $state<string[]>([]);
  let loadingModels = $state(false);

  async function refreshModels() {
    loadingModels = true;
    try {
      ollamaModels = await invoke<string[]>('list_ollama_models');
    } catch {
      ollamaModels = [];
    } finally {
      loadingModels = false;
    }
  }

  $effect(() => {
    if (sub === 'ollama') refreshModels();
  });

  const p = (key: string, fallback = '') => prefs[key] ?? fallback;
</script>

<div class="subtabs">
  {#each (['ollama', 'local', 'cloud'] as const) as t}
    <button class="subtab" class:active={sub === t} onclick={() => sub = t}>
      {t === 'ollama' ? 'Ollama' : t === 'local' ? 'Local GGUF' : 'Cloud API'}
    </button>
  {/each}
</div>

{#if sub === 'ollama'}
  <div class="section">
    <label>Server URL
      <input type="text" value={p('llm.ollama_url', 'http://localhost:11434')}
        onblur={e => save('llm.ollama_url', e.currentTarget.value)} />
    </label>

    <label>
      Model
      <div class="model-row">
        <select value={p('llm.ollama_model', 'qwen2.5:3b')}
          onchange={e => save('llm.ollama_model', e.currentTarget.value)}>
          {#if ollamaModels.length === 0}
            <option value={p('llm.ollama_model', 'qwen2.5:3b')}>
              {p('llm.ollama_model', 'qwen2.5:3b')}
            </option>
          {/if}
          {#each ollamaModels as m}
            <option value={m}>{m}</option>
          {/each}
        </select>
        <button class="refresh" onclick={refreshModels} title="Làm mới danh sách">
          {loadingModels ? '…' : '↻'}
        </button>
      </div>
    </label>
    {#if ollamaModels.length === 0 && !loadingModels}
      <p class="hint">Ollama chưa chạy hoặc chưa có model nào.</p>
    {/if}
  </div>

{:else if sub === 'local'}
  <div class="section">
    <label>Đường dẫn file GGUF
      <input type="text" value={p('llm.llama_model_path')}
        placeholder="D:\haily\models\model.gguf"
        onblur={e => save('llm.llama_model_path', e.currentTarget.value)} />
    </label>
    <label>Prompt format
      <select value={p('llm.llama_prompt_format', 'chatml')}
        onchange={e => save('llm.llama_prompt_format', e.currentTarget.value)}>
        <option value="chatml">ChatML — Qwen2.5</option>
        <option value="gemma4">Gemma4 — Google Gemma</option>
      </select>
    </label>
    <label>Context window (tokens)
      <input type="number" min="512" max="131072" step="512"
        value={p('llm.llama_n_ctx', '4096')}
        onblur={e => save('llm.llama_n_ctx', e.currentTarget.value)} />
    </label>
    <label>GPU layers (0 = CPU-only, 999 = full GPU)
      <input type="number" min="0" max="999"
        value={p('llm.llama_n_gpu_layers', '0')}
        onblur={e => save('llm.llama_n_gpu_layers', e.currentTarget.value)} />
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

  .section { display: flex; flex-direction: column; gap: 16px; }

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

  .model-row { display: flex; gap: 6px; }
  .model-row select { flex: 1; }
  .refresh {
    width: 36px;
    height: 36px;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
    background: #16162a;
    color: #8884aa;
    font-size: 16px;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    transition: color 0.15s;
  }
  .refresh:hover { color: #c084fc; }

  .hint { font-size: 11px; color: #6b6b8a; margin-top: -8px; }
</style>
