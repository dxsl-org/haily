<script lang="ts">
  // One coding-workspace row for the Workspaces screen (Unified Chat UI phase 10, D6). Reads in
  // plain language — no git vocabulary here; `branch`/`worktree_path` are carried on
  // `WorkspaceView` ONLY for the Settings → Advanced disclosure (`AdvancedTab.svelte`), never
  // rendered by this component. `DiffViewer` is embedded inline (view-only) when expanded;
  // Apply/Reject live at the row level so they act without requiring the diff to be open first.
  import { discardWorkspace, resolveApproval, resumeRun, type WorkspaceView, type QueuedApproval } from '$lib/tauri';
  import DiffViewer from '../DiffViewer.svelte';
  import {
    workspaceStatusHint,
    workspaceStatusLabel,
    workspaceTaskLabel,
    type WorkspaceStatusLabel,
  } from './WorkspaceStatus';

  // HTML class attributes split on whitespace — the VN labels themselves contain spaces, so
  // they cannot be used directly as a CSS class token (`class="status-đang chạy"` would parse
  // as two separate classes). This map is presentation-only; the label text itself is what
  // renders and what the no-git-terms test asserts on.
  const STATUS_CLASS: Record<WorkspaceStatusLabel, string> = {
    'đang chạy': 'status-running',
    'chờ áp dụng': 'status-pending',
    'đã áp dụng': 'status-applied',
    'đã dọn dẹp': 'status-reclaimed',
  };

  let {
    workspace,
    matchedApproval,
    onChanged,
  }: { workspace: WorkspaceView; matchedApproval: QueuedApproval | null; onChanged: () => void } = $props();

  let showDiff = $state(false);
  let discarding = $state(false);
  let resuming = $state(false);
  let deciding = $state(false);
  let error = $state('');

  let status = $derived(
    workspaceStatusLabel({
      runStatus: workspace.run_status,
      dirty: workspace.dirty,
      reclaimed: workspace.worktree_reclaimed,
    }),
  );
  let hint = $derived(workspaceStatusHint(status, workspace.run_id !== null));
  let taskLabel = $derived(workspaceTaskLabel(workspace.task));

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

  async function resume() {
    if (resuming || !workspace.resumable || !workspace.run_id) return;
    resuming = true;
    error = '';
    try {
      const ok = await resumeRun(workspace.run_id);
      if (ok) onChanged();
    } catch (e) {
      // `resume_run` already throws a clear, user-facing Vietnamese message for the "already
      // cleaned up / already applied" race (see its own doc comment) — surface it verbatim.
      error = String(e);
    } finally {
      resuming = false;
    }
  }

  async function decide(approved: boolean) {
    if (!matchedApproval || deciding) return;
    deciding = true;
    error = '';
    try {
      await resolveApproval(matchedApproval.session_id, matchedApproval.approval_id, approved);
      onChanged();
    } catch (e) {
      error = String(e);
    } finally {
      deciding = false;
    }
  }
</script>

<div class="row">
  <div class="head">
    <span class="task">Bản làm việc riêng cho {taskLabel} — {workspace.changed_file_count} tệp thay đổi</span>
    <span class="badge {STATUS_CLASS[status]}">{status}</span>
    <span class="badge sandbox">{workspace.sandbox_kind}</span>
  </div>
  <p class="status-hint">{hint}</p>

  {#if !workspace.sandbox_enforcing}
    <div class="null-sandbox-warning">
      ⚠ Không có lớp cách ly (sandbox) trên máy này — mọi lệnh chạy trực tiếp, cần bạn phê duyệt
      thủ công cho mỗi hành động lần đầu.
    </div>
  {/if}

  <div class="actions">
    <button
      class="toggle-diff-btn"
      onclick={() => (showDiff = !showDiff)}
      disabled={workspace.worktree_reclaimed}
    >
      {showDiff ? 'Ẩn thay đổi' : 'Xem thay đổi'}
    </button>
    <button class="apply-btn" onclick={() => decide(true)} disabled={!matchedApproval || deciding}>
      {deciding ? 'Đang áp dụng…' : 'Áp dụng'}
    </button>
    {#if matchedApproval}
      <button class="reject-btn" onclick={() => decide(false)} disabled={deciding}>Từ chối</button>
    {/if}
    <button class="discard-btn" onclick={discard} disabled={discarding}>
      {discarding ? 'Đang huỷ…' : 'Huỷ'}
    </button>
    {#if workspace.resumable}
      <button class="resume-btn" onclick={resume} disabled={resuming}>
        {resuming ? 'Đang tiếp tục…' : 'Tiếp tục'}
      </button>
    {/if}
  </div>
  {#if error}<div class="status-error">⚠️ {error}</div>{/if}

  {#if showDiff && !workspace.worktree_reclaimed}
    <DiffViewer {workspace} />
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
  .task { font-size: 12px; font-weight: 600; color: #e0dff5; }

  .status-hint { margin: 0; font-size: 11px; color: #6b6b8a; }

  .badge {
    font-size: 9px;
    padding: 2px 7px;
    border-radius: 999px;
    background: #1e1e35;
    border: 1px solid #2e2e4a;
    color: #a09ac0;
    white-space: nowrap;
  }
  .badge.status-running { color: #60a5fa; border-color: #1e3a5f; }
  .badge.status-pending { color: #fbbf24; border-color: #7f5a1d; }
  .badge.status-applied { color: #4ade80; border-color: #1e4620; }
  .badge.status-reclaimed { color: #6b6b8a; border-color: #2e2e4a; }

  .null-sandbox-warning {
    font-size: 11px;
    line-height: 1.5;
    color: #f87171;
    padding: 8px;
    background: #2a1017;
    border: 1px solid #7f1d1d;
    border-radius: 7px;
  }

  .actions { display: flex; gap: 8px; flex-wrap: wrap; }

  .toggle-diff-btn, .apply-btn, .reject-btn, .discard-btn, .resume-btn {
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
  .toggle-diff-btn:disabled, .apply-btn:disabled, .reject-btn:disabled,
  .discard-btn:disabled, .resume-btn:disabled { opacity: 0.5; cursor: default; }

  .apply-btn { color: #4ade80; }
  .apply-btn:hover:not(:disabled) { border-color: #1e4620; background: #0f1e13; }

  .reject-btn, .discard-btn { color: #f87171; }
  .reject-btn:hover:not(:disabled), .discard-btn:hover:not(:disabled) {
    border-color: #7f1d1d;
    background: #2a1017;
  }

  .resume-btn { color: #60a5fa; }
  .resume-btn:hover:not(:disabled) { border-color: #1e3a5f; background: #101a2a; }

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
