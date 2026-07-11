<script lang="ts">
  // One coding workspace row for `WorkspacePanel.svelte`. Embeds `DiffViewer` inline when
  // expanded rather than opening a separate modal — keeps the view+accept flow next to
  // the workspace it belongs to.
  import { discardWorkspace, type WorkspaceView, type QueuedApproval } from '$lib/tauri';
  import DiffViewer from './DiffViewer.svelte';

  let {
    workspace,
    matchedApproval,
    onChanged,
  }: { workspace: WorkspaceView; matchedApproval: QueuedApproval | null; onChanged: () => void } = $props();

  let showDiff = $state(false);
  let discarding = $state(false);
  let error = $state('');

  async function discard() {
    if (discarding) return;
    discarding = true;
    error = '';
    try {
      const ok = await discardWorkspace(workspace.id, workspace.session_id);
      if (ok) onChanged();
    } catch (e) {
      error = String(e);
    } finally {
      discarding = false;
    }
  }
</script>

<div class="row">
  <div class="head">
    <span class="branch">{workspace.branch}</span>
    {#if workspace.dirty}<span class="badge dirty">dirty</span>{/if}
    <span class="badge sandbox">{workspace.sandbox_kind}</span>
  </div>

  {#if !workspace.sandbox_enforcing}
    <div class="null-sandbox-warning">
      ⚠ NullSandbox — execution here is NOT isolated. Every first-time tool call in this
      workspace needs your explicit approval.
    </div>
  {/if}

  <div class="meta">
    <span class="path">{workspace.repo_path}</span>
    <span>created {workspace.created_at}</span>
    {#if workspace.work_item_id}<span>work item {workspace.work_item_id}</span>{/if}
  </div>

  <div class="actions">
    <button class="toggle-diff-btn" onclick={() => (showDiff = !showDiff)}>
      {showDiff ? 'Hide diff' : 'View diff'}
    </button>
    <button class="discard-btn" onclick={discard} disabled={discarding}>
      {discarding ? 'Discarding…' : 'Discard'}
    </button>
  </div>
  {#if error}<div class="status-error">⚠️ {error}</div>{/if}

  {#if showDiff}
    <DiffViewer {workspace} {matchedApproval} onResolved={onChanged} />
  {/if}
</div>

<style>
  .row {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 10px;
    background: #0f0f18;
    border: 1px solid #23233a;
    border-radius: 8px;
  }

  .head { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
  .branch { font-size: 12px; font-weight: 600; color: #e0dff5; font-family: ui-monospace, monospace; }

  .badge {
    font-size: 9px;
    padding: 2px 7px;
    border-radius: 999px;
    background: #1e1e35;
    border: 1px solid #2e2e4a;
    color: #a09ac0;
  }
  .badge.dirty { color: #fbbf24; border-color: #7f5a1d; }

  .null-sandbox-warning {
    font-size: 11px;
    line-height: 1.5;
    color: #f87171;
    padding: 8px;
    background: #2a1017;
    border: 1px solid #7f1d1d;
    border-radius: 7px;
  }

  .meta { display: flex; flex-direction: column; gap: 2px; font-size: 10px; color: #6b6b8a; }
  .path { color: #8884aa; word-break: break-all; }

  .actions { display: flex; gap: 8px; }

  .toggle-diff-btn, .discard-btn {
    padding: 5px 12px;
    min-height: 32px;
    border-radius: 7px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #c084fc;
    font-size: 11px;
    cursor: pointer;
  }
  .toggle-diff-btn:hover:not(:disabled) { border-color: #7c3aed; background: #1e1e35; }
  .discard-btn { color: #f87171; }
  .discard-btn:hover:not(:disabled) { border-color: #7f1d1d; background: #2a1017; }
  .discard-btn:disabled { opacity: 0.5; cursor: default; }

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
