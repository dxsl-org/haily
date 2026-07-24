<script lang="ts">
  // Unified per-file diff of a workspace's changes (Unified Chat UI phase 10; absorbed from the
  // Mobile Thin-Client plan's P11b). VIEW ONLY — deep editing is delegated to ACP editors (P12);
  // Haily builds no editor here. Diff text is UNTRUSTED repo content, already tag-stripped and
  // size-capped server-side (`workspaceDiff`'s doc comment) — rendered exclusively as plain text
  // nodes below, never `{@html}`. Apply/Reject moved to the row level (`WorkspaceRow.svelte`) so
  // this component owns exactly one responsibility: showing what changed.
  import { workspaceDiff, type WorkspaceView } from '$lib/tauri';
  import { parseUnifiedDiff, type DiffFile } from '$lib/diff-utils';

  let { workspace }: { workspace: WorkspaceView } = $props();

  let files = $state<DiffFile[]>([]);
  let filesTruncated = $state(false);
  let loading = $state(true);
  let loadError = $state('');

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
</script>

<div class="diff-viewer">
  {#if loading}
    <div class="empty">Đang tải thay đổi…</div>
  {:else if loadError}
    <div class="status-error">⚠️ {loadError}</div>
  {:else if files.length === 0}
    <div class="empty">Không có thay đổi nào trong không gian làm việc này.</div>
  {:else}
    {#if filesTruncated}
      <div class="hint">Có nhiều tệp hơn số hiển thị — chỉ {files.length} tệp đầu được hiển thị.</div>
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
