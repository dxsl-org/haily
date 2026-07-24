<script lang="ts">
  // Fetch + render active coding workspaces for the Workspaces screen (Unified Chat UI phase
  // 10). `listWorkspaces` is a plain read-back, no push channel — refetch on mount + manual
  // refresh (mirrors `ConnectorConfig`'s own pattern). Absorbed from the Mobile Thin-Client
  // plan's `WorkspacePanel.svelte`, which this file replaces.
  import { listWorkspaces, type WorkspaceView, type QueuedApproval } from '$lib/tauri';
  import WorkspaceRow from './WorkspaceRow.svelte';

  // Best-effort correlation source for each row's Apply/Reject actions — passed down from
  // whichever parent also renders the shared approval queue, since both need the same
  // approvals snapshot.
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
    <span class="switch-title">Không gian làm việc</span>
    <button class="refresh-btn" onclick={load} disabled={loading} title="Làm mới">↻</button>
  </div>
  {#if loading}
    <div class="empty">Đang tải…</div>
  {:else if error}
    <div class="status-error">⚠️ {error}</div>
  {:else if workspaces.length === 0}
    <div class="empty">Chưa có không gian làm việc nào đang hoạt động.</div>
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
