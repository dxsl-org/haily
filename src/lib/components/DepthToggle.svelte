<script lang="ts">
  // Phase 7 (Depth Tiers): a small Quick/Normal/Deep toggle with a cost hint. Deep buys
  // multi-stream judgment (judge panel, refuter votes, apex judge) at ~3–5× cost — the
  // backend NEVER auto-escalates to it, so this explicit toggle (or a genuine user-message
  // phrase) is the only way to reach it. The selected mode is persisted server-side and
  // takes effect on the next message.
  import { setDepth, type DepthMode } from '$lib/tauri';

  // Self-managed control: it defaults to Normal and drives the persisted mode on click.
  // The backend re-reads the persisted pref per message, so the toggle does not need to be
  // seeded from a parent prop (which would only capture an initial value anyway).
  let selected = $state<DepthMode>('normal');
  let error = $state('');

  const MODES: { value: DepthMode; label: string; hint: string }[] = [
    { value: 'quick', label: 'Nhanh', hint: 'Ít bước hơn, trả lời nhanh' },
    { value: 'normal', label: 'Thường', hint: 'Cân bằng — mặc định' },
    { value: 'deep', label: 'Sâu', hint: 'Phán đoán đa luồng · chi phí ~3–5×' },
  ];

  async function choose(value: DepthMode) {
    selected = value;
    error = '';
    try {
      await setDepth(value);
    } catch (e) {
      error = String(e);
    }
  }
</script>

<div class="depth-toggle" role="radiogroup" aria-label="Độ sâu phán đoán">
  {#each MODES as m (m.value)}
    <button
      type="button"
      role="radio"
      aria-checked={selected === m.value}
      class:selected={selected === m.value}
      title={m.hint}
      onclick={() => choose(m.value)}
    >
      {m.label}
    </button>
  {/each}
</div>
{#if selected === 'deep'}
  <p class="cost-hint">Chế độ Sâu dùng nhiều lượt hơn — chi phí ước tính cao gấp 3–5 lần.</p>
{/if}
{#if error}
  <p class="error">{error}</p>
{/if}

<style>
  .depth-toggle {
    display: inline-flex;
    gap: 2px;
    border: 1px solid var(--border, #333);
    border-radius: 6px;
    overflow: hidden;
  }
  .depth-toggle button {
    background: transparent;
    border: none;
    padding: 4px 10px;
    cursor: pointer;
    color: var(--fg, #ddd);
    font-size: 0.85rem;
  }
  .depth-toggle button.selected {
    background: var(--accent, #4a6);
    color: #fff;
  }
  .cost-hint {
    margin: 4px 0 0;
    font-size: 0.75rem;
    opacity: 0.75;
  }
  .error {
    color: #d66;
    font-size: 0.75rem;
    margin: 4px 0 0;
  }
</style>
