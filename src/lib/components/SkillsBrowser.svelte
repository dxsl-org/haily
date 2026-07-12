<script lang="ts">
  // Authored + synthesized skills browser (P11b). `listSkills` is a plain read-back, no
  // push channel — refetch on mount and via the manual refresh button, same pattern as
  // `ConnectorConfig.svelte`.
  import { listSkills, type SkillView } from '$lib/tauri';
  import SkillRow from './SkillRow.svelte';

  // Rolling corpus of this session's `StageOutput` text, forwarded by `RunTimeline` via
  // `CockpitView`. `RunEvent` has no `SkillActivated` variant (see tauri.ts), so
  // "activated this run" is a best-effort substring match against skill names rather
  // than an authoritative backend field — documented in `activatedNames` below.
  let { runOutputText = '' }: { runOutputText?: string } = $props();

  let skills = $state<SkillView[]>([]);
  let loading = $state(true);
  let error = $state('');

  $effect(() => {
    load();
  });

  async function load() {
    loading = true;
    error = '';
    try {
      skills = await listSkills();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  const activatedNames = $derived(
    new Set(skills.filter((s) => s.name.length > 0 && runOutputText.includes(s.name)).map((s) => s.name)),
  );
</script>

<div class="section">
  <div class="list-header">
    <span class="switch-title">Skills</span>
    <button class="refresh-btn" onclick={load} disabled={loading} title="Refresh">↻</button>
  </div>
  {#if loading}
    <div class="empty">Loading…</div>
  {:else if error}
    <div class="status-error">⚠️ {error}</div>
  {:else if skills.length === 0}
    <div class="empty">No skills yet.</div>
  {:else}
    <div class="rows">
      {#each skills as skill (skill.name)}
        <SkillRow {skill} activated={activatedNames.has(skill.name)} onChanged={load} />
      {/each}
    </div>
  {/if}
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 10px; }

  .list-header { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
  .switch-title { font-size: 13px; color: #e0dff5; font-weight: 600; }

  .refresh-btn {
    flex-shrink: 0;
    width: 30px;
    padding: 4px 0;
    text-align: center;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
    background: #16162a;
    color: #8884aa;
    font-size: 13px;
    cursor: pointer;
  }
  .refresh-btn:hover:not(:disabled) { color: #c084fc; border-color: #4a3a7a; }
  .refresh-btn:disabled { opacity: 0.5; cursor: default; }

  .rows { display: flex; flex-direction: column; gap: 8px; }

  .empty {
    font-size: 12px;
    color: #6b6b8a;
    padding: 10px;
    background: #0f0f18;
    border-radius: 8px;
    border: 1px dashed #2e2e4a;
  }

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
