<script lang="ts">
  // Run detail drill-in (Unified Chat UI phase 7, D6) — mounts the kept-unused `RunTimeline` +
  // `DiffViewer` (first use since P01) plus `RunTelemetry`. Closes the Track A interim
  // run-detail gap (progress-card-only) the plan accepted at P01. `workspace` is best-effort:
  // resolved by matching `listWorkspaces()` against this run's id (once linked) or session id
  // (before it is) — a run with no coding workspace (e.g. an eval/plan-only run) simply omits
  // the diff section.
  import { listRunEvents, listWorkspaces, type RunEvent, type RunSummary, type WorkspaceView } from '$lib/tauri';
  import RunTimeline from './RunTimeline.svelte';
  import RunTelemetry from './RunTelemetry.svelte';
  import DiffViewer from './DiffViewer.svelte';
  import { runTaskLabel } from '$lib/run-summary';

  let { run, onClose }: { run: RunSummary; onClose: () => void } = $props();

  let seedEvents = $state<RunEvent[]>([]);
  let workspace = $state<WorkspaceView | null>(null);
  let loading = $state(true);
  let loadError = $state('');

  async function load(runId: string, sessionId: string) {
    loading = true;
    loadError = '';
    try {
      const [events, workspaces] = await Promise.all([listRunEvents(runId), listWorkspaces()]);
      seedEvents = events;
      workspace = workspaces.find((w) => w.run_id === runId || w.session_id === sessionId) ?? null;
    } catch (e) {
      loadError = String(e);
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    load(run.id, run.session_id);
  });
</script>

<div class="drill-in">
  <button class="back" onclick={onClose}>← Quay lại danh sách</button>
  <h2>{runTaskLabel(run.task)}</h2>

  {#if loading}
    <p class="empty">Đang tải chi tiết…</p>
  {:else}
    {#if loadError}<p class="status-error">⚠️ {loadError}</p>{/if}

    <RunTimeline runId={run.id} sessionId={run.session_id} {seedEvents} />
    <RunTelemetry {run} />

    {#if workspace}
      <h3>Thay đổi</h3>
      <DiffViewer {workspace} />
    {/if}
  {/if}
</div>

<style>
  .drill-in {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 16px;
    overflow-y: auto;
    flex: 1;
    min-height: 0;
  }

  .back {
    align-self: flex-start;
    padding: 4px 10px;
    min-height: 28px;
    border-radius: 7px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #a09ac0;
    font-size: 11px;
    cursor: pointer;
  }
  .back:hover { border-color: #4a3a7a; color: #c084fc; }

  h2 { font-size: 14px; color: #e0dff5; margin: 0; }
  h3 { font-size: 12px; color: #e0dff5; margin: 0; }

  .empty { font-size: 12px; color: #6b6b8a; }

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
