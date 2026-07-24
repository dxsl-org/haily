<script lang="ts">
  // Inline collapsed progress card for one pipeline run, rendered directly in the chat
  // flow of the session that launched it (D6). Driven entirely by the in-memory
  // `run-events.ts` reducer state — there is no persisted-row reconcile yet (that lands
  // with P05's `run_events` table + P07's Runs screen), so a run started before this GUI
  // window observed it, or while the user was on a different route, will not appear here.
  // Accepted interim gap (plan.md D6 / phase-04 Overview): this card is the ONLY run-detail
  // surface in Track A.
  import { killRun } from '$lib/tauri';
  import { escalationCount, formatElapsed, retryCount, type Job } from '$lib/run-events';
  import { narrate } from '$lib/run-narration';
  import RunEventLog from './RunEventLog.svelte';

  // `onOpenRun` (Unified Chat UI phase 7): navigates to the Runs screen's drill-in for this
  // run — provided by `+page.svelte` (the only place holding both `route` and `jobsState`).
  // `undefined` (e.g. an isolated Storybook-style render) leaves "Chi tiết →" disabled, same
  // as its pre-P07 inert state.
  let { job, onOpenRun }: { job: Job; onOpenRun?: (runId: string) => void } = $props();

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
    // A synthesized `RunComplete{outcome:"interrupted"}` marker must render distinctly from a
    // genuine failure/success (review MED — the Runs-row already shows orange "Gián đoạn").
    interrupted: 'Gián đoạn',
  };

  const isActive = $derived(job.status === 'running' || job.status === 'paused');
  const elapsedMs = $derived((job.completedAt ?? now) - job.startedAt);
  const lastEvent = $derived(job.events[job.events.length - 1]);
  const lastLine = $derived(lastEvent ? narrate(lastEvent) : 'Đang khởi chạy tác vụ');
  const retries = $derived(retryCount(job));
  const escalations = $derived(escalationCount(job));
  const title = $derived(job.workItemId || `tác vụ …${job.runId.slice(-8)}`);

  // Stops THIS run specifically via `kill_run` (Unified Chat UI phase 6, D3) — run-id-
  // addressed, unlike the earlier `cancelTurn(sessionId)` this replaces (P07 deferred
  // rewiring, per P06's Deviation Log): a session can host only one pipeline run at a time
  // in practice, but `kill_run` no longer relies on that being true.
  async function stop() {
    if (stopping || !isActive) return;
    stopping = true;
    try {
      await killRun(job.runId);
    } catch (e) {
      console.error('killRun failed', e);
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
    <!-- Wired to the Runs drill-in (Unified Chat UI phase 7, D6) — disabled only when the
         caller provided no navigation callback. -->
    <button
      class="details-link"
      disabled={!onOpenRun}
      onclick={() => onOpenRun?.(job.runId)}
      title="Xem đầy đủ trong màn hình Tác vụ"
    >
      Chi tiết →
    </button>
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
  .status-interrupted .status { color: #fb923c; }

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
    color: #c084fc;
    margin-left: auto;
  }
  .details-link:disabled { color: #4a4a6a; cursor: default; }
</style>
