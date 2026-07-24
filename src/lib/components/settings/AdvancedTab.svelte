<script lang="ts">
  // Settings → Advanced (Unified Chat UI phase 10, D6) — the ONE place the default Workspaces
  // screen's git-specific fields (branch, worktree path) are ever shown to the user. Everything
  // elsewhere in the app must stay plain-language; this tab is deliberately the technical
  // escape hatch for a user who wants to inspect the underlying git state directly.
  import { onMount } from 'svelte';
  import { listWorkspaces, type WorkspaceView } from '$lib/tauri';

  let workspaces = $state<WorkspaceView[]>([]);
  let loading = $state(true);
  let error = $state('');

  onMount(load);

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
</script>

<div class="section">
  <div class="block">
    <div class="switch-copy">
      <span class="switch-title">Chi tiết kỹ thuật không gian làm việc</span>
      <span class="hint">
        Đường dẫn thư mục làm việc (worktree) và tên nhánh (branch) git nội bộ mà Haily dùng cho
        từng không gian làm việc — chỉ dành cho việc gỡ lỗi thủ công.
      </span>
    </div>
    <button class="refresh-btn" onclick={load} disabled={loading}>
      {loading ? 'Đang tải…' : 'Làm mới'}
    </button>

    {#if error}
      <div class="status-error">⚠️ {error}</div>
    {:else if !loading && workspaces.length === 0}
      <div class="hint">Chưa có không gian làm việc nào đang hoạt động.</div>
    {:else}
      <div class="rows">
        {#each workspaces as w (w.id)}
          <div class="row">
            <span class="label">Nhánh (branch)</span>
            <span class="value">{w.branch}</span>
            <span class="label">Thư mục làm việc (worktree)</span>
            <span class="value">{w.worktree_path}</span>
          </div>
        {/each}
      </div>
    {/if}
  </div>
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 20px; }
  .block { display: flex; flex-direction: column; gap: 10px; }

  .switch-copy { display: flex; flex-direction: column; gap: 4px; }
  .switch-title { font-size: 13px; color: #e0dff5; font-weight: 600; }
  .hint { font-size: 11px; color: #6b6b8a; line-height: 1.5; }

  .refresh-btn {
    align-self: flex-start;
    padding: 5px 12px;
    border: 1px solid #2e2e4a;
    border-radius: 7px;
    background: #16162a;
    color: #c084fc;
    font-size: 11px;
    cursor: pointer;
  }
  .refresh-btn:hover:not(:disabled) { border-color: #7c3aed; background: #1e1e35; }
  .refresh-btn:disabled { opacity: 0.5; cursor: default; }

  .rows { display: flex; flex-direction: column; gap: 8px; }
  .row {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 10px;
    background: #0f0f18;
    border: 1px solid #23233a;
    border-radius: 8px;
    font-size: 11px;
  }
  .label { color: #6b6b8a; margin-top: 4px; }
  .label:first-child { margin-top: 0; }
  .value {
    color: #a09ac0;
    font-family: ui-monospace, monospace;
    word-break: break-all;
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
