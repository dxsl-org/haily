<script lang="ts">
  // One run row for the Runs screen (Unified Chat UI phase 7, D6) — status color, task, plain-
  // verb last-output line, "cần bạn" (needs-you) badge, timestamp, row-level Stop/Resume. Click
  // anywhere on the row except a button opens the drill-in (`onOpen`).
  import type { RunRowView } from '$lib/run-summary';

  let {
    view,
    onOpen,
    onStop,
    onResume,
    busy,
  }: {
    view: RunRowView;
    onOpen: () => void;
    onStop: () => Promise<void>;
    onResume: () => Promise<void>;
    busy: boolean;
  } = $props();

  const stoppable = $derived(view.status === 'queued' || view.status === 'running' || view.status === 'paused');

  let acting = $state(false);

  async function stop(e: MouseEvent) {
    e.stopPropagation();
    if (acting) return;
    acting = true;
    try {
      await onStop();
    } finally {
      acting = false;
    }
  }

  async function resume(e: MouseEvent) {
    e.stopPropagation();
    if (acting) return;
    acting = true;
    try {
      await onResume();
    } finally {
      acting = false;
    }
  }

  function formatTimestamp(iso: string): string {
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? iso : d.toLocaleString('vi-VN');
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      onOpen();
    }
  }
</script>

<!-- A `<div role="button">`, not a nested `<button>` — the row itself opens the drill-in but
     contains its OWN Stop/Resume buttons, and a `<button>` cannot host a descendant `<button>`
     (SSR hydration-mismatch warning). -->
<div
  class="row status-{view.status}"
  role="button"
  tabindex="0"
  onclick={onOpen}
  onkeydown={onKeydown}
>
  <div class="head">
    <span class="badge">{view.statusBadge}</span>
    <span class="task">{view.taskLabel}</span>
    {#if view.needsYou}
      <span class="needs-you">Cần bạn</span>
    {/if}
    <span class="timestamp">{formatTimestamp(view.updatedAt)}</span>
  </div>
  <p class="last-line">{view.lastLine}</p>
  <div class="actions">
    {#if stoppable}
      <button class="stop" onclick={stop} disabled={acting || busy}>
        {acting ? 'Đang dừng…' : '■ Dừng'}
      </button>
    {/if}
    {#if view.resumable}
      <button class="resume" onclick={resume} disabled={acting || busy}>
        {acting ? 'Đang tiếp tục…' : 'Tiếp tục'}
      </button>
    {/if}
  </div>
</div>

<style>
  .row {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 10px 12px;
    border-radius: 10px;
    background: #14142a;
    border: 1px solid #2e2e4a;
    font-size: 12px;
    text-align: left;
    cursor: pointer;
    width: 100%;
    font-family: inherit;
    color: inherit;
  }
  .row:hover { border-color: #4a3a7a; }

  .head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }

  .badge {
    flex-shrink: 0;
    font-size: 10px;
    font-weight: 700;
    padding: 2px 8px;
    border-radius: 999px;
    background: #1e1e35;
    color: #a09ac0;
  }
  .status-running .badge, .status-queued .badge { color: #c084fc; }
  .status-paused .badge { color: #fbbf24; }
  .status-interrupted .badge { color: #fb923c; }
  .status-done .badge { color: #4ade80; }
  .status-failed .badge { color: #f87171; }

  .task {
    color: #e0dff5;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
    min-width: 0;
  }

  .needs-you {
    flex-shrink: 0;
    font-size: 10px;
    font-weight: 700;
    padding: 2px 8px;
    border-radius: 999px;
    background: #3a1f2e;
    color: #f87171;
  }

  .timestamp { flex-shrink: 0; color: #6b6b8a; font-size: 11px; }

  .last-line { color: #a09ac0; margin: 0; }

  .actions { display: flex; gap: 8px; }

  .actions button {
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
  .resume { background: #101a2a; color: #60a5fa; }
  .resume:hover:not(:disabled) { background: #16223a; }
  .actions button:disabled { opacity: 0.5; cursor: default; }
</style>
