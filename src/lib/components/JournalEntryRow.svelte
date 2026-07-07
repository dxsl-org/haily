<script lang="ts">
  // One row of the journal/audit browser (`JournalBrowser.svelte`) — split out to keep
  // both files under the project's ~200-line guideline and because a row is fully
  // self-contained (its own expand/collapse + undo state never needs to be known by
  // the parent list).
  //
  // Undo still goes through the EXISTING chat shim (`sendMessage` naming the journal
  // id) — the LLM turns it into a `journal_undo` tool call through the normal
  // approval-gated, kill-switch-exempt compensation path. No new write path.
  import { sendMessage, type JournalEntry } from '$lib/tauri';
  import { toolVerb, extractDiff } from '$lib/tool-verbs';

  let { entry }: { entry: JournalEntry } = $props();

  let expanded = $state(false);

  function statusLabel(e: JournalEntry): string {
    switch (e.undoStatus) {
      case 'undone':
        return 'Đã hoàn tác';
      case 'stuck':
        return 'Kẹt — cần xử lý thủ công';
      case 'compensation_failed':
        return 'Hoàn tác thất bại — có thể thử lại';
      case 'refused':
        return 'Từ chối hoàn tác';
      case 'undo_requested':
      case 'compensating':
        return 'Đang hoàn tác…';
      default:
        return 'Chưa hoàn tác';
    }
  }

  // Only these two undo_status values are a real invitation to retry — every other
  // value is either terminal (undone/refused) or already in flight.
  function canUndo(e: JournalEntry): boolean {
    return e.undoStatus === 'not_requested' || e.undoStatus === 'compensation_failed';
  }

  let undoing = $state(false);
  let undoError = $state('');
  async function requestUndo() {
    if (undoing) return;
    undoing = true;
    undoError = '';
    try {
      await sendMessage(`Undo the action with journal id "${entry.id}".`);
    } catch (e) {
      undoError = String(e);
    } finally {
      undoing = false;
    }
  }
</script>

<div class="entry">
  <button class="entry-main" onclick={() => (expanded = !expanded)}>
    <span class="entry-verb">{toolVerb(entry.toolName, entry.requestParams)}</span>
    <span class="entry-status">{statusLabel(entry)}</span>
  </button>
  <div class="entry-meta">{entry.createdAt} · <code>{entry.toolName}</code></div>

  {#if expanded}
    <div class="detail">
      <div class="detail-row"><span class="detail-label">Đọc lại:</span> {entry.readbackStatus}</div>
      {#if entry.manifestHash}
        <div class="detail-row">
          <span class="detail-label">Manifest:</span>
          <code>{entry.manifestHash.slice(0, 12)}…</code>
        </div>
      {/if}
      {#each extractDiff(entry.toolName, entry.preState, entry.postState) as field (field.label)}
        <div class="detail-row">
          <span class="detail-label">{field.label}:</span>
          <span class="diff-before">{field.before}</span> → <span class="diff-after">{field.after}</span>
        </div>
      {/each}
      {#if entry.undoStatus === 'stuck'}
        <div class="stuck-plan">
          <span class="hint">Không thể tự động hoàn tác. Kế hoạch để xử lý thủ công:</span>
          <pre>{entry.compensationPlan ?? '(không có)'}</pre>
        </div>
      {/if}
    </div>
  {/if}

  {#if canUndo(entry)}
    <button class="undo-btn" onclick={requestUndo} disabled={undoing}>
      {undoing ? 'Đang hoàn tác…' : 'Hoàn tác'}
    </button>
  {/if}
  {#if undoError}
    <div class="status-error">⚠️ {undoError}</div>
  {/if}
</div>

<style>
  .hint { font-size: 11px; color: #6b6b8a; line-height: 1.5; }

  .entry {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 10px;
    background: #0f0f18;
    border: 1px solid #23233a;
    border-radius: 8px;
  }
  .entry-main {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 8px;
    border: none;
    background: transparent;
    padding: 0;
    cursor: pointer;
    text-align: left;
    font: inherit;
    width: 100%;
  }
  .entry-verb { color: #e0dff5; font-size: 12px; }
  .entry-status { font-size: 11px; color: #8884aa; flex-shrink: 0; }
  .entry-meta { font-size: 10px; color: #4a4a6a; }
  .entry-meta code { color: #6b6b8a; }

  .detail { display: flex; flex-direction: column; gap: 4px; margin-top: 4px; padding-top: 6px; border-top: 1px solid #1e1e2e; }
  .detail-row { font-size: 11px; color: #a09ac0; }
  .detail-label { color: #6b6b8a; margin-right: 4px; }
  .diff-before { color: #f87171; }
  .diff-after { color: #4ade80; }

  .stuck-plan { display: flex; flex-direction: column; gap: 4px; margin-top: 4px; }
  .stuck-plan pre {
    background: #16162a;
    border: 1px solid #2a2a45;
    border-radius: 6px;
    padding: 8px;
    font-size: 11px;
    color: #f87171;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 120px;
    overflow: auto;
  }

  .undo-btn {
    align-self: flex-start;
    margin-top: 4px;
    padding: 5px 12px;
    border: 1px solid #2e2e4a;
    border-radius: 7px;
    background: #16162a;
    color: #c084fc;
    font-size: 11px;
    cursor: pointer;
    transition: border-color 0.15s, background 0.15s;
  }
  .undo-btn:hover:not(:disabled) { border-color: #7c3aed; background: #1e1e35; }
  .undo-btn:disabled { opacity: 0.5; cursor: default; }

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
