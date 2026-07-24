<script lang="ts">
  // One run's full ordered event timeline for the Runs drill-in (Unified Chat UI phase 7, D6) —
  // seeded from `list_run_events` (persisted, reconciled against `pipeline_runs.status` server-
  // side), then overlaid by the live `haily-run-events` stream for as long as this component
  // stays mounted. Repurposed from the multi-run cockpit list this file used to render (kept
  // unused since P01, per the plan's D6 decision) — this is its first real mount, now scoped to
  // ONE run rather than every run this GUI window has observed.
  import { onDestroy, onMount } from 'svelte';
  import { onRunEvents, type RunEvent, type RunEventPayload } from '$lib/tauri';
  import { applyRunEvent, type Job } from '$lib/run-events';
  import RunJobCard from './RunJobCard.svelte';

  let { runId, sessionId, seedEvents }: { runId: string; sessionId: string; seedEvents: RunEvent[] } = $props();

  let job = $state<Job | null>(null);
  let unlisten: (() => void) | undefined;

  function seed() {
    let jobs = new Map<string, Job>();
    for (const ev of seedEvents) {
      jobs = applyRunEvent(jobs, sessionId, ev);
    }
    job = jobs.get(runId) ?? null;
  }

  // Re-seeds whenever the parent switches the drill-in target or refreshes the seed.
  $effect(() => {
    seed();
  });

  onMount(() => {
    const unlistenPromise = onRunEvents(({ event }: RunEventPayload) => {
      if (event.data.run_id !== runId) return;
      const jobs = job ? new Map<string, Job>([[runId, job]]) : new Map<string, Job>();
      job = applyRunEvent(jobs, sessionId, event).get(runId) ?? null;
    });
    unlisten = () => { unlistenPromise.then((fn) => fn()); };
  });

  onDestroy(() => unlisten?.());
</script>

<div class="run-timeline">
  {#if job}
    <RunJobCard {job} isExpanded={true} onToggle={() => {}} />
  {:else}
    <div class="empty">Chưa có sự kiện nào được ghi nhận cho lượt chạy này.</div>
  {/if}
</div>

<style>
  .run-timeline { display: flex; flex-direction: column; gap: 8px; }

  .empty {
    font-size: 12px;
    color: #6b6b8a;
    padding: 10px;
    background: #0f0f18;
    border-radius: 8px;
    border: 1px dashed #2e2e4a;
  }
</style>
