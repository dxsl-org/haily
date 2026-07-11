<script lang="ts">
  // One run's expandable timeline card, split out of `RunTimeline.svelte` (mirrors the
  // JournalBrowser/JournalEntryRow split) since a card is fully self-contained state
  // (only its own expand toggle, owned by the parent so job order survives re-renders).
  import { describeEvent, type Job } from '$lib/run-events';

  let { job, isExpanded, onToggle }: { job: Job; isExpanded: boolean; onToggle: () => void } = $props();

  // Virtualize/cap (phase-11 risk note): a long build can emit thousands of StageOutput
  // events — only render the most recent MAX_RENDERED so the DOM can't grow unbounded.
  // The full ordered log still lives in `job.events`; nothing is dropped from the model,
  // only from what's painted.
  const MAX_RENDERED = 400;
  const visibleEvents = $derived(
    job.events.length > MAX_RENDERED ? job.events.slice(job.events.length - MAX_RENDERED) : job.events,
  );
  const truncatedCount = $derived(Math.max(0, job.events.length - MAX_RENDERED));

  const STATUS_LABEL: Record<Job['status'], string> = {
    running: 'Running',
    paused: 'Paused',
    complete: 'Complete',
    failed: 'Failed',
  };
</script>

<div class="job">
  <button class="job-head" onclick={onToggle} aria-expanded={isExpanded}>
    <span class="status status-{job.status}">{STATUS_LABEL[job.status]}</span>
    <span class="title">{job.workItemId || `run …${job.runId.slice(-8)}`}</span>
    {#if job.currentStage}
      <span class="stage">{job.currentStage}{job.currentTier ? ` · ${job.currentTier}` : ''}</span>
    {/if}
    {#if job.lastAttempt !== null && job.lastAttempt > 0}
      <span class="attempt">retry #{job.lastAttempt}</span>
    {/if}
    <span class="chevron">{isExpanded ? '▾' : '▸'}</span>
  </button>

  {#if isExpanded}
    <div class="events">
      {#if truncatedCount > 0}
        <div class="truncated">… {truncatedCount} earlier events not shown</div>
      {/if}
      {#each visibleEvents as event, i (i)}
        {@const d = describeEvent(event)}
        <div class="event tone-{d.tone}">
          <span class="icon">{d.icon}</span>
          <span class="text">{d.text}</span>
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .job {
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 10px;
    overflow: hidden;
  }

  .job-head {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 10px 12px;
    border: none;
    background: transparent;
    cursor: pointer;
    text-align: left;
    font: inherit;
    min-height: 44px;
  }

  .status {
    flex-shrink: 0;
    font-size: 10px;
    font-weight: 700;
    padding: 2px 8px;
    border-radius: 999px;
    background: #1e1e35;
    color: #a09ac0;
  }
  .status-running { color: #c084fc; }
  .status-paused { color: #fbbf24; }
  .status-complete { color: #4ade80; }
  .status-failed { color: #f87171; }

  .title {
    color: #e0dff5;
    font-size: 12px;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .stage, .attempt {
    font-size: 11px;
    color: #6b6b8a;
    flex-shrink: 0;
  }

  .chevron { margin-left: auto; color: #6b6b8a; flex-shrink: 0; }

  .events {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 8px 12px 12px;
    border-top: 1px solid #1e1e2e;
    max-height: 420px;
    overflow-y: auto;
  }

  .truncated { font-size: 10px; color: #4a4a6a; padding: 2px 0; }

  /* Event text is untrusted repo/tool output — rendered via {expression} above, never
     {@html}, so nothing here can inject markup. */
  .event {
    display: flex;
    gap: 6px;
    font-size: 11px;
    line-height: 1.5;
    color: #a09ac0;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .event .icon { flex-shrink: 0; }
  .event.tone-pass { color: #4ade80; }
  .event.tone-fail { color: #f87171; }
  .event.tone-warn { color: #fbbf24; }
</style>
