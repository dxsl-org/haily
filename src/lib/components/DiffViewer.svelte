<script lang="ts">
  // Unified per-file diff of a workspace's changes (P11b). VIEW + ACCEPT ONLY — deep
  // editing is delegated to ACP editors (P12); Haily builds no editor here. Diff text is
  // UNTRUSTED repo content, already tag-stripped and size-capped server-side
  // (`workspaceDiff`'s doc comment) — rendered exclusively as plain text nodes below,
  // never `{@html}`. Accept/Reject reuse the EXISTING `resolveApproval` command (same one
  // the chat `ApprovalModal` calls for `worktree_apply`) — no new backend command, per
  // the P11a deviation log ("Diff-accept = existing worktree_apply approval").
  import { workspaceDiff, resolveApproval, type WorkspaceView, type QueuedApproval } from '$lib/tauri';
  import { parseUnifiedDiff, type DiffFile } from '$lib/diff-utils';

  let {
    workspace,
    matchedApproval = null,
    onResolved,
  }: {
    workspace: WorkspaceView;
    matchedApproval?: QueuedApproval | null;
    onResolved?: () => void;
  } = $props();

  let files = $state<DiffFile[]>([]);
  let filesTruncated = $state(false);
  let loading = $state(true);
  let loadError = $state('');
  let resolving = $state(false);

  $effect(() => {
    load(workspace.id, workspace.session_id);
  });

  async function load(id: string, sessionId: string) {
    loading = true;
    loadError = '';
    try {
      const raw = await workspaceDiff(id, sessionId);
      const parsed = parseUnifiedDiff(raw ?? '');
      files = parsed.files;
      filesTruncated = parsed.filesTruncated;
    } catch (e) {
      loadError = String(e);
    } finally {
      loading = false;
    }
  }

  // `matchedApproval` is a best-effort correlation (by session id) done by the caller —
  // it is NOT guaranteed to be the `worktree_apply` request specifically (the queue
  // snapshot carries no tool name, see `QueuedApproval`'s doc comment), so the label
  // below stays generic rather than claiming certainty it doesn't have.
  async function decide(approved: boolean) {
    if (!matchedApproval || resolving) return;
    resolving = true;
    try {
      await resolveApproval(matchedApproval.session_id, matchedApproval.approval_id, approved);
      onResolved?.();
    } finally {
      resolving = false;
    }
  }
</script>

<div class="diff-viewer">
  {#if loading}
    <div class="empty">Loading diff…</div>
  {:else if loadError}
    <div class="status-error">⚠️ {loadError}</div>
  {:else if files.length === 0}
    <div class="empty">No changes in this workspace.</div>
  {:else}
    {#if filesTruncated}
      <div class="hint">Diff has more files than shown — only the first {files.length} are rendered.</div>
    {/if}
    <div class="file-list">
      {#each files as file (file.path)}
        <details class="file" open>
          <summary>{file.path}</summary>
          <div class="lines">
            {#each file.lines as line, i (i)}
              <div class="line {line.kind}">{line.text}</div>
            {/each}
          </div>
        </details>
      {/each}
    </div>
  {/if}

  {#if matchedApproval}
    <div class="accept-row">
      <span class="hint">There's a pending approval on this workspace's session.</span>
      <div class="actions">
        <button class="deny" onclick={() => decide(false)} disabled={resolving}>Reject</button>
        <button class="approve" onclick={() => decide(true)} disabled={resolving}>Accept</button>
      </div>
    </div>
  {/if}
</div>

<style>
  .diff-viewer {
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-height: 480px;
    overflow-y: auto;
  }

  .empty, .hint {
    font-size: 11px;
    color: #6b6b8a;
    line-height: 1.5;
  }

  .file-list { display: flex; flex-direction: column; gap: 6px; }

  .file {
    background: #0f0f18;
    border: 1px solid #23233a;
    border-radius: 8px;
    overflow: hidden;
  }

  summary {
    padding: 6px 10px;
    font-size: 11px;
    color: #c084fc;
    cursor: pointer;
    font-family: ui-monospace, monospace;
  }

  /* Wide diff lines scroll inside this container, never the page. */
  .lines {
    overflow-x: auto;
    border-top: 1px solid #1e1e2e;
  }

  .line {
    padding: 1px 10px;
    font-size: 11px;
    font-family: ui-monospace, monospace;
    white-space: pre;
    color: #a09ac0;
  }
  .line.add { background: #0f1e13; color: #4ade80; }
  .line.remove { background: #23131a; color: #f87171; }
  .line.meta { color: #4a4a6a; }

  .accept-row {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 10px;
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
  }

  .actions { display: flex; gap: 8px; }

  .actions button {
    padding: 6px 14px;
    min-height: 32px;
    border-radius: 7px;
    border: none;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .actions button:disabled { opacity: 0.5; cursor: default; }
  .deny { background: #2a2a45; color: #ddd8f5; }
  .approve { background: #7c3aed; color: #fff; }

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
