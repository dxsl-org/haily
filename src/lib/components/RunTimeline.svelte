<script lang="ts">
  // Live per-run stage/gate tree, consuming the ordered `haily-run-events` stream (P11a).
  // Multi-run job model (phase-11 architecture note): a coding run is a long-lived job
  // with its own timeline, not a chat bubble — jobs are listed newest-first, each
  // expandable into its full ordered event log via `RunJobCard`.
  import { onMount, onDestroy } from 'svelte';
  import { onRunEvents, type RunEventPayload } from '$lib/tauri';
  import { applyRunEvent, orderedJobs, type Job } from '$lib/run-events';
  import RunJobCard from './RunJobCard.svelte';

  // Forwards raw `StageOutput` text upward so `SkillsBrowser` can best-effort derive
  // "activated this run" — see `CockpitView`'s doc comment for why this lives here
  // rather than a backend field (no `SkillActivated` RunEvent variant exists).
  let { onOutputText }: { onOutputText?: (text: string) => void } = $props();

  let jobs = $state<Map<string, Job>>(new Map());
  let expanded = $state<Set<string>>(new Set());
  let unlisten: (() => void) | undefined;

  onMount(() => {
    // No `list_runs` reconcile command exists yet (P11a deviation log: no GUI-session-
    // bound pipeline run has launched through this bridge). Unlike `WorkItemsPanel`'s
    // fetch-then-subscribe pattern, this timeline is purely event-sourced from mount
    // time forward — a run started before this component mounted will not appear.
    const unlistenPromise = onRunEvents(({ session_id, event }: RunEventPayload) => {
      jobs = applyRunEvent(jobs, session_id, event);
      if (event.type === 'StageOutput') onOutputText?.(event.data.chunk);
      if (event.type === 'RunStarted') {
        expanded = new Set(expanded).add(event.data.run_id);
      }
    });
    unlisten = () => { unlistenPromise.then((fn) => fn()); };
  });

  onDestroy(() => unlisten?.());

  function toggle(runId: string) {
    const next = new Set(expanded);
    if (next.has(runId)) next.delete(runId); else next.add(runId);
    expanded = next;
  }

  const list = $derived(orderedJobs(jobs));
</script>

<div class="run-timeline">
  <h2>Coding runs</h2>
  {#if list.length === 0}
    <div class="empty">No run has started yet in this session.</div>
  {:else}
    <div class="jobs">
      {#each list as job (job.runId)}
        <RunJobCard {job} isExpanded={expanded.has(job.runId)} onToggle={() => toggle(job.runId)} />
      {/each}
    </div>
  {/if}
</div>

<style>
  .run-timeline { display: flex; flex-direction: column; gap: 10px; }

  h2 {
    font-size: 13px;
    font-weight: 600;
    color: #e0dff5;
    margin: 0;
  }

  .jobs { display: flex; flex-direction: column; gap: 8px; }

  .empty {
    font-size: 12px;
    color: #6b6b8a;
    padding: 10px;
    background: #0f0f18;
    border-radius: 8px;
    border: 1px dashed #2e2e4a;
  }
</style>
