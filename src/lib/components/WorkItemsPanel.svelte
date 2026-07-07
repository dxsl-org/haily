<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { listWorkItems, onWorkItemsChanged, type WorkItemStatus } from '$lib/tauri';

  const ICONS: Record<string, string> = {
    running: '⚙',
    queued: '⏳',
    paused: '⏸',
    interrupted: '⏸',
  };

  function icon(status: string): string {
    return ICONS[status] ?? '•';
  }

  // Mirrors the CLI's `render_status_panel` truncation exactly (cli.rs) so the two
  // surfaces read identically for the same title.
  function truncate(title: string): string {
    return title.length > 42 ? `${title.slice(0, 41)}…` : title;
  }

  let items = $state<WorkItemStatus[]>([]);
  let unlisten: (() => void) | undefined;

  onMount(() => {
    // Authoritative fill. Live updates below arrive over a latest-wins watch channel
    // that can silently drop an intermediate (or even the final) snapshot under load
    // — every mount/remount must re-fetch rather than trust accumulated event state,
    // per the phase-5 coalesce/drop policy (also inherited by phase 08).
    listWorkItems()
      .then((snapshot) => { items = snapshot; })
      .catch((e) => console.error('listWorkItems failed', e));

    const unlistenPromise = onWorkItemsChanged((snapshot) => { items = snapshot; });
    unlisten = () => { unlistenPromise.then((fn) => fn()); };
  });

  onDestroy(() => unlisten?.());
</script>

{#if items.length > 0}
  <div class="work-items" role="status" aria-label="Công việc đang chạy">
    {#each items as item, i (i)}
      <div class="item">
        <span class="icon">{icon(item.status)}</span>
        <span class="title">{truncate(item.title)}</span>
        {#if item.phase}<span class="phase">[{item.phase}]</span>{/if}
        <span class="progress">{item.progress}%</span>
      </div>
    {/each}
  </div>
{/if}

<style>
  .work-items {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 8px 16px;
    border-bottom: 1px solid #1e1e2e;
    flex-shrink: 0;
  }

  .item {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: #a8a3c9;
  }

  .icon { color: #c084fc; }

  /* Work-item titles come from user/task content — rendered as plain text nodes
     (never bound via {@html}) so nothing in a title can inject markup (XSS). */
  .title {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .phase { color: #6b6b8a; }

  .progress {
    margin-left: auto;
    color: #6b6b8a;
    font-variant-numeric: tabular-nums;
  }
</style>
