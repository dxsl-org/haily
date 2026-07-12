<script lang="ts">
  // Active CodingWorkspaces (P11b). `listWorkspaces` is a plain read-back, no push
  // channel — refetch on mount + manual refresh, same pattern as `ConnectorConfig`.
  import { listWorkspaces, type WorkspaceView, type QueuedApproval } from '$lib/tauri';
  import WorkspaceRow from './WorkspaceRow.svelte';

  // Best-effort correlation source for each row's embedded `DiffViewer` accept action —
  // owned by the parent (`CockpitView`) since it's shared with `ApprovalsQueue`.
  let { approvals = [] }: { approvals?: QueuedApproval[] } = $props();

  let workspaces = $state<WorkspaceView[]>([]);
  let loading = $state(true);
  let error = $state('');

  $effect(() => {
    load();
  });

  async function load() {
    loading = true;
    error = '';
    try {
      workspaces = await listWorkspaces();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  function matchFor(w: WorkspaceView): QueuedApproval | null {
    return approvals.find((a) => a.session_id === w.session_id) ?? null;
  }
</script>

<div class="section">
  <div class="list-header">
    <span class="switch-title">Workspaces</span>
    <button class="refresh-btn" onclick={load} disabled={loading} title="Refresh">↻</button>
  </div>
  {#if loading}
    <div class="empty">Loading…</div>
  {:else if error}
    <div class="status-error">⚠️ {error}</div>
  {:else if workspaces.length === 0}
    <div class="empty">No active coding workspaces.</div>
  {:else}
    <div class="rows">
      {#each workspaces as w (w.id)}
        <WorkspaceRow workspace={w} matchedApproval={matchFor(w)} onChanged={load} />
      {/each}
    </div>
  {/if}
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 10px; }

  .list-header { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
  .switch-title { font-size: 13px; color: #e0dff5; font-weight: 600; }

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
    font-size: 12px;
    color: #6b6b8a;
    padding: 10px;
    background: #0f0f18;
    border-radius: 8px;
    border: 1px dashed #2e2e4a;
  }

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
