<script lang="ts">
  // Inline collapsed progress card for one pipeline run, rendered directly in the chat
  // flow of the session that launched it (D6). Driven entirely by the in-memory
  // `run-events.ts` reducer state — there is no persisted-row reconcile yet (that lands
  // with P05's `run_events` table + P07's Runs screen), so a run started before this GUI
  // window observed it, or while the user was on a different route, will not appear here.
  // Accepted interim gap (plan.md D6 / phase-04 Overview): this card is the ONLY run-detail
  // surface in Track A.
  import { cancelTurn } from '$lib/tauri';
  import { escalationCount, formatElapsed, retryCount, type Job } from '$lib/run-events';
  import { narrate } from '$lib/run-narration';
  import RunEventLog from './RunEventLog.svelte';

  let { job }: { job: Job } = $props();

  let expanded = $state(false);
  let stopping = $state(false);

  // Ticks once a second so the elapsed label stays live while running. Review fix
  // LOW-4: an `$effect` (not a plain `onMount` interval) so the ticker actually STOPS
  // once `job.completedAt` is set, rather than continuing to fire uselessly for the rest
  // of the card's (potentially long-lived) mounted lifetime — with `jobsState` now
  // page-lifetime (MED-1), a session can accumulate many finished cards, each of which
  // would otherwise carry its own dead interval for as long as the app runs.
  let now = $state(Date.now());
  $effect(() => {
    if (job.completedAt !== null) return;
    const handle = setInterval(() => {
      now = Date.now();
    }, 1000);
    return () => clearInterval(handle);
  });

  const STATUS_LABEL: Record<Job['status'], string> = {
    running: 'Đang chạy',
    paused: 'Tạm dừng',
    complete: 'Hoàn tất',
    failed: 'Thất bại',
  };

  const isActive = $derived(job.status === 'running' || job.status === 'paused');
  const elapsedMs = $derived((job.completedAt ?? now) - job.startedAt);
  const lastEvent = $derived(job.events[job.events.length - 1]);
  const lastLine = $derived(lastEvent ? narrate(lastEvent) : 'Đang khởi chạy tác vụ');
  const retries = $derived(retryCount(job));
  const escalations = $derived(escalationCount(job));
  const title = $derived(job.workItemId || `tác vụ …${job.runId.slice(-8)}`);

  // Stops the run via the session's cancellation token — the launching turn's own token
  // is reused for the whole pipeline run (`trigger.rs:140`), so this is a coarse
  // per-session stop, not yet run-id-addressed. P06 (`kill_run`) upgrades this to target
  // only this run without touching anything else in the session.
  async function stop() {
    if (stopping || !isActive) return;
    stopping = true;
    try {
      await cancelTurn(job.sessionId);
    } catch (e) {
      console.error('cancelTurn failed', e);
    } finally {
      stopping = false;
    }
  }
</script>

<div class="card status-{job.status}">
  <button class="head" onclick={() => (expanded = !expanded)} aria-expanded={expanded}>
    <span class="status">{STATUS_LABEL[job.status]}</span>
    <span class="title">{title}</span>
    {#if job.currentStage}
      <span class="stage">{job.currentStage}</span>
    {/if}
    <span class="elapsed">{formatElapsed(elapsedMs)}</span>
    {#if retries > 0}
      <span class="counter" title="Số lần thử lại">↻ {retries}</span>
    {/if}
    {#if escalations > 0}
      <span class="counter" title="Số lần nâng cấp mô hình">⇧ {escalations}</span>
    {/if}
    <span class="chevron">{expanded ? '▾' : '▸'}</span>
  </button>

  <!-- `lastLine` is model-neutral fixed VN vocabulary from `narrate()`, never raw
       tool/model payload — safe to render as plain text (no {@html} anywhere here). -->
  <p class="last-line">{lastLine}</p>

  <div class="footer">
    {#if isActive}
      <button class="stop" onclick={stop} disabled={stopping}>{stopping ? 'Đang dừng…' : '■ Dừng'}</button>
    {/if}
    <!-- Inert until P07 wires the Runs drill-in (plan.md D6) — rendered now so the
         affordance's final position doesn't shift layout when it activates. -->
    <button class="details-link" disabled title="Sắp có — xem đầy đủ trong màn hình Tác vụ">Chi tiết →</button>
  </div>

  {#if expanded}
    <RunEventLog events={job.events} />
  {/if}
</div>

<style>
  .card {
    align-self: flex-start;
    max-width: 92%;
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 8px 12px;
    border-radius: 12px;
    background: #14142a;
    border: 1px solid #2e2e4a;
    font-size: 12px;
  }

  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    border: none;
    background: transparent;
    color: inherit;
    cursor: pointer;
    text-align: left;
    font: inherit;
    padding: 2px 0;
    min-height: 32px;
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
  .status-running .status { color: #c084fc; }
  .status-paused .status { color: #fbbf24; }
  .status-complete .status { color: #4ade80; }
  .status-failed .status { color: #f87171; }

  .title {
    color: #e0dff5;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .stage, .elapsed, .counter { color: #6b6b8a; flex-shrink: 0; }
  .chevron { margin-left: auto; color: #6b6b8a; flex-shrink: 0; }

  .last-line { color: #a09ac0; padding-left: 2px; }

  .footer { display: flex; align-items: center; gap: 8px; }

  .footer button {
    padding: 4px 10px;
    min-height: 28px;
    border-radius: 7px;
    border: none;
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
  }

  .stop { background: #3a1f2e; color: #f87171; }
  .stop:hover:not(:disabled) { background: #4a2436; }
  .stop:disabled { opacity: 0.5; cursor: default; }

  .details-link {
    background: transparent;
    color: #4a4a6a;
    cursor: default;
    margin-left: auto;
  }
</style>
