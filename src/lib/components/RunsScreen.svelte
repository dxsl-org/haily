<script lang="ts">
  // Runs management screen (Unified Chat UI phase 7, D6) — flat list (live + history from
  // `pipeline_runs`) with a drill-in that mounts `RunTimeline`/`DiffViewer`/`RunTelemetry`.
  // `initialRunId` lets a caller (the chat progress card's "Chi tiết →") deep-link straight
  // into a run's drill-in on navigation; consumed once via `onInitialRunConsumed` so a later
  // route switch back to Runs doesn't re-open it unexpectedly.
  import type { RunJobsState } from '$lib/run-jobs-state.svelte';
  import { listRuns, type RunSummary } from '$lib/tauri';
  import RunsList from './RunsList.svelte';
  import RunDrillIn from './RunDrillIn.svelte';

  let {
    jobsState,
    initialRunId = null,
    onInitialRunConsumed,
  }: {
    jobsState: RunJobsState;
    initialRunId?: string | null;
    onInitialRunConsumed?: () => void;
  } = $props();

  let selected = $state<RunSummary | null>(null);

  $effect(() => {
    if (!initialRunId) return;
    const id = initialRunId;
    listRuns()
      .then((runs) => {
        const match = runs.find((r) => r.id === id);
        if (match) selected = match;
      })
      .finally(() => onInitialRunConsumed?.());
  });
</script>

<div class="screen">
  {#if selected}
    <RunDrillIn run={selected} onClose={() => (selected = null)} />
  {:else}
    <RunsList {jobsState} onOpenRun={(run) => (selected = run)} />
  {/if}
</div>

<style>
  .screen {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
</style>
