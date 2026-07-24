<script lang="ts">
  // One coding-workspace row for the Workspaces screen (Unified Chat UI phase 10, D6). Reads in
  // plain language — no git vocabulary here; `branch`/`worktree_path` are carried on
  // `WorkspaceView` ONLY for the Settings → Advanced disclosure (`AdvancedTab.svelte`), never
  // rendered by this component. `DiffViewer` is embedded inline (view-only) when expanded.
  //
  // Review MED follow-up: a matched approval is correlated by `session_id` ALONE (`QueuedApproval`
  // carries no tool name), so it is never safe to label it as "the diff-apply request" — this row
  // shows only the generic `ROW_COPY.pendingApprovalNotice` and points to Chat, where the real
  // `ToolApprovalRequest` payload (tool name + args) is available. Apply/Reject are NOT offered
  // here (dropped from the earlier row-level promotion — see the phase's Deviation Log).
  import { discardWorkspace, resumeRun, type WorkspaceView, type QueuedApproval } from '$lib/tauri';
  import DiffViewer from '../DiffViewer.svelte';
  import {
    ROW_COPY,
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
  let error = $state('');

  let status = $derived(
    workspaceStatusLabel({
      runStatus: workspace.run_status,
      dirty: workspace.dirty,
      reclaimed: workspace.worktree_reclaimed,
    }),
  );
  let hint = $derived(workspaceStatusHint(status, workspace.run_status));
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
</script>

<div class="row">
  <div class="head">
    <span class="task">{ROW_COPY.taskPrefix} {taskLabel} — {workspace.changed_file_count} {ROW_COPY.changedFilesSuffix}</span>
    <span class="badge {STATUS_CLASS[status]}">{status}</span>
    <span class="badge sandbox">{workspace.sandbox_kind}</span>
  </div>
  <p class="status-hint">{hint}</p>

  {#if !workspace.sandbox_enforcing}
    <div class="null-sandbox-warning">{ROW_COPY.nullSandboxWarning}</div>
  {/if}

  {#if matchedApproval}
    <p class="pending-approval-notice">{ROW_COPY.pendingApprovalNotice}</p>
  {/if}

  <div class="actions">
    <button
      class="toggle-diff-btn"
      onclick={() => (showDiff = !showDiff)}
      disabled={workspace.worktree_reclaimed}
    >
      {showDiff ? ROW_COPY.hideDiff : ROW_COPY.showDiff}
    </button>
    <button class="discard-btn" onclick={discard} disabled={discarding}>
      {discarding ? ROW_COPY.discarding : ROW_COPY.discard}
    </button>
    {#if workspace.resumable}
      <button class="resume-btn" onclick={resume} disabled={resuming}>
        {resuming ? ROW_COPY.resuming : ROW_COPY.resume}
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

  .pending-approval-notice {
    margin: 0;
    font-size: 11px;
    line-height: 1.5;
    color: #fbbf24;
    padding: 8px;
    background: #2a1f0f;
    border: 1px solid #7f5a1d;
    border-radius: 7px;
  }

  .actions { display: flex; gap: 8px; flex-wrap: wrap; }

  .toggle-diff-btn, .discard-btn, .resume-btn {
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
  .toggle-diff-btn:disabled, .discard-btn:disabled, .resume-btn:disabled { opacity: 0.5; cursor: default; }

  .discard-btn { color: #f87171; }
  .discard-btn:hover:not(:disabled) { border-color: #7f1d1d; background: #2a1017; }

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
