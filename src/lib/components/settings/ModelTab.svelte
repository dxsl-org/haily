<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';
  import { reloadLlm } from '$lib/tauri';

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
      const provider = await reloadLlm();
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

  // ── Cost/quality routing dial (Auto Model Routing R1, phase 7) ──────────────

  let costQuality = $state(7);
  let costQualitySaving = $state(false);
  let costQualityError = $state('');

  // `routing_enabled` absent must read as ON — mirrors the backend's own boot-time
  // seed default (`haily-core`'s `routing_enabled_pref` comment: "absent preference
  // must default to ON"), so a fresh install shows the toggle already engaged.
  let routingEnabled = $state(true);
  let routingSaving = $state(false);

  $effect(() => {
    const cq = prefs['llm.cost_quality'];
    if (cq !== undefined) {
      const n = Number(cq);
      if (Number.isFinite(n)) costQuality = n;
    }
    const re = prefs['llm.routing_enabled'];
    if (re !== undefined) routingEnabled = re === 'true' || re === '1';
  });

  // Persists then hot-swaps the router at the next turn boundary (mirrors `applyCloud`).
  // Mid-drag `input` events only update the label; the actual save/reload fires once on
  // `change` (drag release) to avoid spamming `reload_llm` on every tick.
  async function applyCostQuality(value: number) {
    costQuality = value;
    costQualityError = '';
    costQualitySaving = true;
    try {
      await save('llm.cost_quality', String(value));
      await reloadLlm();
    } catch (e) {
      costQualityError = String(e);
    } finally {
      costQualitySaving = false;
    }
  }

  // No `reload_llm` here: `llm.routing_enabled` rides Phase 4's live special-case in
  // `set_preference` (the SAME `Arc<AtomicBool>` every `TurnRuntime` reads), so the
  // toggle takes effect on the very next turn with no router swap needed.
  async function toggleRouting() {
    const next = !routingEnabled;
    routingEnabled = next;
    routingSaving = true;
    try {
      await save('llm.routing_enabled', next ? 'true' : 'false');
    } finally {
      routingSaving = false;
    }
  }

  // ── Cloud API multi-key management ──────────────────────────────────────────

  // Editable list of API keys for the cloud tab.
  let apiKeys = $state<string[]>([]);
  let cloudApplyError = $state('');
  let cloudApplyOk = $state('');
  let cloudApplying = $state(false);

  // Populate from prefs once they arrive (can be empty on first load).
  $effect(() => {
    const json = prefs['llm.cloud_api_keys'];
    if (json) {
      try { apiKeys = JSON.parse(json); } catch {}
    } else if (prefs['llm.cloud_api_key']) {
      // backward compat: migrate single-key pref
      apiKeys = [prefs['llm.cloud_api_key']];
    }
  });

  function addKey() { apiKeys = [...apiKeys, '']; }
  function removeKey(i: number) { apiKeys = apiKeys.filter((_, idx) => idx !== i); }
  function updateKey(i: number, val: string) {
    apiKeys = apiKeys.map((k, idx) => (idx === i ? val : k));
  }

  // ── Per-tier model config (hybrid multi-model) ──────────────────────────────
  // Each routing tier names one cloud model; base_url/keys left blank inherit the
  // defaults above (aggregator/single-provider case), or are overridden per tier
  // (direct multi-provider case). Persisted as JSON under `llm.tier.<tier>`.

  type TierKey = 'fast' | 'medium' | 'thinking' | 'ultra';
  const TIER_ORDER: TierKey[] = ['fast', 'medium', 'thinking', 'ultra'];
  const TIER_LABELS: Record<TierKey, string> = {
    fast: 'Nhanh / rẻ',
    medium: 'Cân bằng',
    thinking: 'Suy luận',
    ultra: 'Cao cấp',
  };

  interface TierCfg { model: string; baseUrl: string; apiKeys: string[]; showOverride: boolean; }
  const emptyTier = (): TierCfg => ({ model: '', baseUrl: '', apiKeys: [], showOverride: false });
  let tiers = $state<Record<TierKey, TierCfg>>({
    fast: emptyTier(), medium: emptyTier(), thinking: emptyTier(), ultra: emptyTier(),
  });

  // Populate from prefs. New JSON `llm.tier.<t>` wins; legacy plain-model
  // `llm.tier_model.<t>` (Phase-3 foundation) is a backward-compat fallback.
  $effect(() => {
    for (const t of TIER_ORDER) {
      const json = prefs[`llm.tier.${t}`];
      if (json) {
        try {
          const o = JSON.parse(json);
          if (o && typeof o.model === 'string') {
            const baseUrl = typeof o.base_url === 'string' ? o.base_url : '';
            const keys = Array.isArray(o.api_keys) ? o.api_keys.filter((k: unknown) => typeof k === 'string') : [];
            tiers[t] = { model: o.model, baseUrl, apiKeys: keys, showOverride: Boolean(baseUrl.trim() || keys.length) };
            continue;
          }
        } catch {}
      }
      const legacy = prefs[`llm.tier_model.${t}`];
      if (legacy) tiers[t] = { model: legacy, baseUrl: '', apiKeys: [], showOverride: false };
    }
  });

  function addTierKey(t: TierKey) { tiers[t].apiKeys = [...tiers[t].apiKeys, '']; }
  function removeTierKey(t: TierKey, i: number) { tiers[t].apiKeys = tiers[t].apiKeys.filter((_, idx) => idx !== i); }
  function updateTierKey(t: TierKey, i: number, val: string) {
    tiers[t].apiKeys = tiers[t].apiKeys.map((k, idx) => (idx === i ? val : k));
  }

  async function applyCloud() {
    cloudApplyError = '';
    cloudApplyOk = '';
    cloudApplying = true;
    try {
      const filtered = apiKeys.map(k => k.trim()).filter(Boolean);
      await save('llm.cloud_api_keys', JSON.stringify(filtered));

      // Persist each tier's model + optional own endpoint/keys. A blank model clears the
      // tier; the legacy plain-string pref is always cleared so it can never resurrect and
      // shadow the new JSON schema.
      let tierCount = 0;
      for (const t of TIER_ORDER) {
        const model = tiers[t].model.trim();
        if (model) {
          const blob: Record<string, unknown> = { model };
          const bu = tiers[t].baseUrl.trim();
          if (bu) blob.base_url = bu;
          const keys = tiers[t].apiKeys.map(k => k.trim()).filter(Boolean);
          if (keys.length) blob.api_keys = keys;
          await save(`llm.tier.${t}`, JSON.stringify(blob));
          tierCount++;
        } else {
          await save(`llm.tier.${t}`, '');
        }
        await save(`llm.tier_model.${t}`, '');
      }

      const provider = await reloadLlm();
      cloudApplyOk = provider !== 'unconfigured'
        ? `✓ Đã áp dụng — ${filtered.length} key, ${tierCount} model theo tier, provider: ${provider}`
        : '⚠️ Không có key hợp lệ — kiểm tra lại API key.';
    } catch (e) {
      cloudApplyError = String(e);
    } finally {
      cloudApplying = false;
    }
  }
</script>

<div class="section routing-section">
  <label class="toggle-row">
    <span>Tự chọn model theo tác vụ</span>
    <input
      type="checkbox"
      checked={routingEnabled}
      disabled={routingSaving}
      onchange={toggleRouting}
    />
  </label>
  <span class="hint">
    Khi bật, Haily tự chọn model theo độ khó từng tác vụ (rẻ ↔ giỏi) thay vì luôn dùng model cấu hình cố định. Có hiệu lực từ tin nhắn sau, không cần khởi động lại.
  </span>

  <label>Ưu tiên rẻ ↔ giỏi
    <input
      type="range"
      min="0" max="10" step="1"
      value={costQuality}
      oninput={e => costQuality = Number(e.currentTarget.value)}
      onchange={e => applyCostQuality(Number(e.currentTarget.value))}
    />
    <div class="slider-labels">
      <span>Tiết kiệm</span>
      <span>Cân bằng</span>
      <span>Chất lượng</span>
    </div>
  </label>
  <span class="hint">Áp dụng từ tin nhắn sau — kéo xong thả tay để lưu.</span>

  {#if costQualitySaving}
    <div class="status-info">⏳ Đang áp dụng…</div>
  {:else if costQualityError}
    <div class="status-error">⚠️ {costQualityError}</div>
  {/if}
</div>

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
        value={p('llm.llama_n_ctx', '8192')}
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
    <label>Model mặc định
      <input type="text" value={p('llm.cloud_model', 'gpt-4o-mini')}
        onblur={e => save('llm.cloud_model', e.currentTarget.value)} />
      <span class="hint">Dùng khi tắt auto-routing, hoặc làm fallback cho tier chưa gán model.</span>
    </label>

    <div class="key-section">
      <div class="key-header">
        <span class="key-label">API Keys mặc định <span class="key-count">({apiKeys.length})</span></span>
        <button class="add-btn" onclick={addKey}>+ Thêm key</button>
      </div>
      {#if apiKeys.length === 0}
        <div class="key-empty">Chưa có API key nào. Bấm "+ Thêm key" để thêm.</div>
      {:else}
        <div class="key-list">
          {#each apiKeys as key, i}
            <div class="key-row">
              <span class="key-idx">{i + 1}</span>
              <input
                type="password"
                placeholder="sk-..."
                value={key}
                oninput={e => updateKey(i, e.currentTarget.value)}
              />
              <button class="del-btn" onclick={() => removeKey(i)} title="Xóa key này">✕</button>
            </div>
          {/each}
        </div>
      {/if}
      <span class="hint">
        Nhiều key luân phiên theo vòng tròn. Nếu một key bị rate-limit (429), key kế tiếp được dùng tự động.
      </span>
    </div>

    <div class="tier-section">
      <div class="tier-title">
        Model theo tier <span class="tier-sub">(khi bật auto-routing)</span>
      </div>
      {#each TIER_ORDER as t}
        <div class="tier-row">
          <span class="tier-label">{TIER_LABELS[t]}</span>
          <input
            class="tier-model"
            type="text"
            placeholder="để trống = dùng model mặc định"
            value={tiers[t].model}
            oninput={e => tiers[t].model = e.currentTarget.value}
          />
          <button
            class="ovr-btn"
            class:active={tiers[t].showOverride}
            onclick={() => tiers[t].showOverride = !tiers[t].showOverride}
            title="Endpoint / key riêng cho tier này"
          >⚙</button>
        </div>
        {#if tiers[t].showOverride}
          <div class="tier-override">
            <label>Base URL riêng
              <input
                type="text"
                placeholder="trống = dùng Base URL mặc định"
                value={tiers[t].baseUrl}
                oninput={e => tiers[t].baseUrl = e.currentTarget.value}
              />
              <span class="hint">Nên là endpoint OpenAI-compatible (OpenAI, Groq, Together, OpenRouter, vLLM…). Anthropic/Google native chưa chạy đủ trên mọi luồng.</span>
            </label>
            <div class="key-section">
              <div class="key-header">
                <span class="key-label">API Keys riêng <span class="key-count">({tiers[t].apiKeys.length})</span></span>
                <button class="add-btn" onclick={() => addTierKey(t)}>+ Thêm key</button>
              </div>
              {#if tiers[t].apiKeys.length}
                <div class="key-list">
                  {#each tiers[t].apiKeys as key, i}
                    <div class="key-row">
                      <span class="key-idx">{i + 1}</span>
                      <input
                        type="password"
                        placeholder="sk-..."
                        value={key}
                        oninput={e => updateTierKey(t, i, e.currentTarget.value)}
                      />
                      <button class="del-btn" onclick={() => removeTierKey(t, i)} title="Xóa key này">✕</button>
                    </div>
                  {/each}
                </div>
              {/if}
              <span class="hint">Trống = dùng key mặc định ở trên.</span>
            </div>
          </div>
        {/if}
      {/each}
      <span class="hint">
        Mỗi tier gọi 1 model. Để trống endpoint/key = kế thừa mặc định (đủ cho OpenRouter). Điền riêng khi dùng provider trực tiếp khác nhau.
      </span>
    </div>

    <button class="apply-btn" onclick={applyCloud} disabled={cloudApplying}>
      {cloudApplying ? '⏳ Đang áp dụng…' : 'Áp dụng'}
    </button>

    {#if cloudApplyError}
      <div class="status-error">⚠️ {cloudApplyError}</div>
    {:else if cloudApplyOk}
      <div class="status-ok">{cloudApplyOk}</div>
    {/if}
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

  .routing-section {
    padding-bottom: 18px;
    margin-bottom: 4px;
    border-bottom: 1px solid #2e2e4a;
  }
  .toggle-row {
    flex-direction: row !important;
    align-items: center;
    justify-content: space-between;
    font-size: 13px;
    color: #e0dff5;
  }
  .toggle-row input[type="checkbox"] {
    width: auto;
    accent-color: #7c3aed;
  }
  input[type="range"] {
    padding: 0;
    accent-color: #7c3aed;
  }
  .slider-labels {
    display: flex;
    justify-content: space-between;
    font-size: 10px;
    color: #6b6b8a;
  }

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

  .key-section { display: flex; flex-direction: column; gap: 8px; }
  .key-header { display: flex; justify-content: space-between; align-items: center; }
  .key-label { font-size: 12px; color: #8884aa; }
  .key-count { color: #6b6b8a; font-size: 11px; }
  .key-list { display: flex; flex-direction: column; gap: 6px; }
  .key-row { display: flex; align-items: center; gap: 6px; }
  .key-idx {
    flex-shrink: 0;
    width: 18px;
    font-size: 11px;
    color: #4a4a6a;
    text-align: right;
  }
  .key-row input { flex: 1; }
  .del-btn {
    flex-shrink: 0;
    width: 28px;
    height: 28px;
    border: 1px solid #2e2e4a;
    border-radius: 6px;
    background: #16162a;
    color: #6b6b8a;
    font-size: 11px;
    cursor: pointer;
    transition: color 0.15s, border-color 0.15s;
    display: flex; align-items: center; justify-content: center;
  }
  .del-btn:hover { color: #f87171; border-color: #7f1d1d; }
  .add-btn {
    font-size: 11px;
    padding: 4px 10px;
    border: 1px solid #2e2e4a;
    border-radius: 6px;
    background: #16162a;
    color: #a09ac0;
    cursor: pointer;
    transition: color 0.15s, border-color 0.15s;
  }
  .add-btn:hover { color: #c084fc; border-color: #4a3a7a; }
  .key-empty {
    font-size: 12px;
    color: #4a4a6a;
    padding: 8px 10px;
    background: #0f0f18;
    border-radius: 8px;
    border: 1px dashed #2e2e4a;
  }
  .tier-section {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding-top: 14px;
    border-top: 1px solid #2e2e4a;
  }
  .tier-title { font-size: 12px; color: #8884aa; }
  .tier-sub { color: #4a4a6a; font-size: 11px; }
  .tier-row { display: flex; align-items: center; gap: 6px; }
  .tier-label {
    flex-shrink: 0;
    width: 82px;
    font-size: 11px;
    color: #a09ac0;
  }
  .tier-model { flex: 1; }
  .ovr-btn {
    flex-shrink: 0;
    width: 30px;
    height: 30px;
    border: 1px solid #2e2e4a;
    border-radius: 6px;
    background: #16162a;
    color: #6b6b8a;
    font-size: 13px;
    cursor: pointer;
    transition: color 0.15s, border-color 0.15s;
    display: flex; align-items: center; justify-content: center;
  }
  .ovr-btn:hover { color: #c084fc; border-color: #4a3a7a; }
  .ovr-btn.active { color: #c084fc; border-color: #4c1d95; background: #1e1635; }
  .tier-override {
    display: flex;
    flex-direction: column;
    gap: 12px;
    margin: 2px 0 6px 88px;
    padding: 10px;
    background: #0f0f18;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
  }

  .apply-btn {
    padding: 8px 14px;
    border: none;
    border-radius: 8px;
    background: #4c1d95;
    color: #e0dff5;
    font-size: 13px;
    cursor: pointer;
    transition: background 0.15s;
    align-self: flex-start;
  }
  .apply-btn:hover:not(:disabled) { background: #6d28d9; }
  .apply-btn:disabled { opacity: 0.5; cursor: default; }

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
