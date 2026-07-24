<script lang="ts">
  // Flat run list for the Runs screen (Unified Chat UI phase 7, D6, D3) — every active run plus
  // recent history from `pipeline_runs`, overlaid by the live per-session job reducer (P04)
  // keyed by run id. Polls on a fixed interval (mirrors `WorkspacesScreen`'s own approval poll)
  // since there is no push channel for the persisted list itself — a live run's own status
  // still updates instantly via the reducer overlay between polls.
  import { onDestroy, onMount } from 'svelte';
  import { killRun, listRuns, resumeRun, type RunSummary } from '$lib/tauri';
  import { toRowView } from '$lib/run-summary';
  import type { RunJobsState } from '$lib/run-jobs-state.svelte';
  import RunRow from './RunRow.svelte';

  const POLL_MS = 4000;

  let { jobsState, onOpenRun }: { jobsState: RunJobsState; onOpenRun: (run: RunSummary) => void } = $props();

  let runs = $state<RunSummary[]>([]);
  let loading = $state(true);
  let loadError = $state('');
  let pollHandle: ReturnType<typeof setInterval> | undefined;

  async function load() {
    try {
      runs = await listRuns();
      loadError = '';
    } catch (e) {
      loadError = String(e);
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    load();
    pollHandle = setInterval(load, POLL_MS);
  });

  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
  });

  const rows = $derived(runs.map((run) => ({ run, view: toRowView(run, jobsState.jobs.get(run.id)) })));

  async function stop(runId: string) {
    try {
      await killRun(runId);
    } finally {
      await load();
    }
  }

  async function resume(runId: string) {
    try {
      await resumeRun(runId);
    } finally {
      await load();
    }
  }
</script>

<div class="section">
  <div class="list-header">
    <span class="title">Tác vụ</span>
    <button class="refresh-btn" onclick={load} disabled={loading} title="Làm mới">↻</button>
  </div>
  {#if loading}
    <div class="empty">Đang tải…</div>
  {:else if loadError}
    <div class="status-error">⚠️ {loadError}</div>
  {:else if rows.length === 0}
    <div class="empty">
      <span class="glyph">▶</span>
      <p>Chưa có tác vụ nào.</p>
      <p class="hint">Các tác vụ chạy pipeline sẽ xuất hiện ở đây.</p>
    </div>
  {:else}
    <div class="rows">
      {#each rows as { run, view } (run.id)}
        <RunRow
          {view}
          busy={loading}
          onOpen={() => onOpenRun(run)}
          onStop={() => stop(run.id)}
          onResume={() => resume(run.id)}
        />
      {/each}
    </div>
  {/if}
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 10px; padding: 16px; overflow-y: auto; flex: 1; min-height: 0; }

  .list-header { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
  .title { font-size: 13px; color: #e0dff5; font-weight: 600; }

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
    text-align: center;
    color: #6b6b8a;
    padding: 24px 10px;
    background: #0f0f18;
    border-radius: 8px;
    border: 1px dashed #2e2e4a;
  }
  .empty .glyph { display: block; font-size: 28px; margin-bottom: 8px; opacity: 0.6; }
  .empty p { margin: 4px 0; font-size: 13px; }
  .empty .hint { font-size: 12px; color: #4a4a6a; }

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
