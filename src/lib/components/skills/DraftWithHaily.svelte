<script lang="ts">
  // "Nhờ Haily soạn" (D4: draft-with-Haily). Reuses the ORDINARY chat/turn path (`sendMessage`
  // + the existing `haily-chunk` stream) — there is no dedicated draft command — so the reply
  // also appears in the main Chat conversation like any other turn; this panel independently
  // filters `haily-chunk` by the session id `sendMessage` returns rather than depending on
  // anything owned by `ChatStream`/`+page.svelte` (neither of which this phase may touch).
  //
  // NEVER auto-saves: `onFill` only replaces the caller's in-memory draft fields. Saving is the
  // caller's own explicit `Lưu` action.
  import { sendMessage, cancelTurn, onChunk, type Chunk, type ChunkPayload, type SkillDraft } from '$lib/tauri';
  import { buildDraftPrompt, parseDraftMarkdown, stripCodeFence } from '$lib/skill-draft-format';

  let { onFill }: { onFill: (draft: SkillDraft) => void } = $props();

  let description = $state('');
  let drafting = $state(false);
  let error = $state('');
  let accumulated = $state('');
  let sessionId: string | null = null;
  let unlisten: (() => void) | null = null;

  function stopListening() {
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  }

  function finish() {
    drafting = false;
    stopListening();
    if (!error && accumulated.trim().length > 0) {
      onFill(parseDraftMarkdown(stripCodeFence(accumulated)));
    }
    sessionId = null;
  }

  function applyChunk(chunk: Chunk) {
    if (chunk.type === 'Text') {
      accumulated += chunk.data;
    } else if (chunk.type === 'Complete') {
      finish();
    } else if (chunk.type === 'Error') {
      error = chunk.data;
      finish();
    }
  }

  async function requestDraft() {
    if (drafting || description.trim().length === 0) return;
    drafting = true;
    error = '';
    accumulated = '';

    // Subscribe BEFORE dispatching the turn: `sendMessage` can start emitting chunks the
    // instant its underlying invoke lands, before its own promise resolves with the session
    // id — subscribing only after `await sendMessage(...)` would drop that gap's Text chunks
    // and yield a truncated draft. Chunks that arrive before we know our own session id are
    // buffered and replayed once it's known, instead of being matched against `undefined`.
    let targetSid: string | null = null;
    const pending: ChunkPayload[] = [];
    unlisten = await onChunk((payload) => {
      if (targetSid === null) {
        pending.push(payload);
        return;
      }
      if (payload.session_id !== targetSid) return;
      applyChunk(payload.chunk);
    });

    try {
      targetSid = await sendMessage(buildDraftPrompt(description.trim()));
      sessionId = targetSid;
      for (const payload of pending) {
        if (payload.session_id === targetSid) applyChunk(payload.chunk);
      }
    } catch (e) {
      error = String(e);
      drafting = false;
      stopListening();
    }
  }

  async function cancel() {
    if (sessionId) {
      await cancelTurn(sessionId);
    }
    // The turn still emits its normal terminal chunk afterward (Complete or Error) — `finish()`
    // runs from that, same contract `cancelTurn`'s own doc comment documents for chat turns.
  }

  $effect(() => () => stopListening());
</script>

<div class="draft-panel">
  <p class="hint">
    Mô tả kỹ năng bằng lời của bạn, Haily sẽ soạn nội dung cho 4 ô bên trên. Bạn cần xem lại và
    bấm "Lưu" thì mới được ghi lại — nội dung soạn ở đây KHÔNG tự động lưu.
  </p>
  <textarea
    bind:value={description}
    rows="3"
    placeholder="Ví dụ: kỹ năng xuất hoá đơn từ Odoo ra file Excel mỗi cuối tháng"
    disabled={drafting}
  ></textarea>
  <div class="actions">
    <button class="draft-btn" onclick={requestDraft} disabled={drafting || description.trim().length === 0}>
      {drafting ? 'Đang soạn…' : 'Nhờ Haily soạn'}
    </button>
    {#if drafting}
      <button class="cancel-btn" onclick={cancel}>Hủy</button>
    {/if}
  </div>
  {#if drafting}
    <div class="preview" aria-live="polite">{accumulated || 'Đang chờ Haily trả lời…'}</div>
  {/if}
  {#if error}<div class="status-error">⚠️ {error}</div>{/if}
</div>

<style>
  .draft-panel {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 10px;
    background: #0f0f18;
    border: 1px dashed #4a3a7a;
    border-radius: 8px;
  }
  .hint { font-size: 11px; color: #8884aa; line-height: 1.5; }

  textarea {
    resize: vertical;
    padding: 8px;
    border-radius: 6px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #e0dff5;
    font-size: 12px;
    font-family: inherit;
  }
  textarea:disabled { opacity: 0.6; }

  .actions { display: flex; gap: 8px; }
  .draft-btn, .cancel-btn {
    padding: 6px 14px;
    min-height: 32px;
    border-radius: 7px;
    border: 1px solid #4a3a7a;
    background: #1e1e35;
    color: #c084fc;
    font-size: 12px;
    cursor: pointer;
  }
  .draft-btn:disabled { opacity: 0.5; cursor: default; }
  .cancel-btn { color: #f87171; border-color: #7f1d1d; }

  .preview {
    max-height: 160px;
    overflow-y: auto;
    white-space: pre-wrap;
    font-size: 11px;
    color: #a09ac0;
    padding: 8px;
    background: #0a0a12;
    border-radius: 6px;
    border: 1px solid #23233a;
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
