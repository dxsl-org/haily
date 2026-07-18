<script lang="ts">
  // Render-quality-independent demand affordances (View Engine Phase A, design §14 — the
  // Phase-B go/no-go signal): a disabled-styled "Edit" button + one-line micro-prompt, and a
  // usefulness thumb pair. Kept as its own component so `WorkspacePane` stays a thin
  // orchestrator; this owns only the prompt's open/closed UI state, never the telemetry call
  // itself (that needs the view/session ids `WorkspacePane` holds).
  let {
    onEditDemand,
    onThumb,
    openedAt,
  }: {
    /** Called ONLY with a non-empty, trimmed answer — an empty/cancelled prompt calls
     * nothing (see `submitEditPrompt`): "a click alone is not demand" (red-team CRITICAL). */
    onEditDemand: (answer: string) => void;
    onThumb: (direction: '+' | '-') => void;
    openedAt: number | null;
  } = $props();

  let editPromptOpen = $state(false);
  let editAnswer = $state('');

  function openEditPrompt() {
    editAnswer = '';
    editPromptOpen = true;
  }

  function cancelEditPrompt() {
    editPromptOpen = false;
    editAnswer = '';
  }

  function submitEditPrompt() {
    const answer = editAnswer.trim();
    editPromptOpen = false;
    editAnswer = '';
    if (!answer) return;
    onEditDemand(answer);
  }

  function onEditKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter') {
      e.preventDefault();
      submitEditPrompt();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      cancelEditPrompt();
    }
  }

  function formatOpenedAt(ts: number): string {
    return new Date(ts).toLocaleTimeString('vi-VN', { hour: '2-digit', minute: '2-digit' });
  }
</script>

<div class="measure-row">
  <!-- "Disabled-styled" per phase spec — muted/low-key so it doesn't read as a primary
       action, but it IS a real, clickable button (a genuine HTML `disabled` button could
       never open the micro-prompt). -->
  <button class="edit-btn" onclick={openEditPrompt} title="Đề xuất chỉnh sửa (thử nghiệm)">✎ Sửa</button>
  <div class="thumbs" role="group" aria-label="Xem dạng này có ích không?">
    <button class="thumb-btn" onclick={() => onThumb('+')} aria-label="Có ích">👍</button>
    <button class="thumb-btn" onclick={() => onThumb('-')} aria-label="Không có ích">👎</button>
  </div>
  {#if openedAt}
    <span class="opened-at">Mở lúc {formatOpenedAt(openedAt)}</span>
  {/if}
</div>

{#if editPromptOpen}
  <div class="edit-prompt-row">
    <input
      type="text"
      class="edit-input"
      placeholder="Bạn muốn sửa gì ở đây?"
      bind:value={editAnswer}
      onkeydown={onEditKeydown}
    />
    <button class="edit-submit" onclick={submitEditPrompt} disabled={!editAnswer.trim()}>Gửi</button>
    <button class="edit-cancel" onclick={cancelEditPrompt}>Hủy</button>
  </div>
{/if}

<style>
  .measure-row {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .edit-btn {
    padding: 5px 10px;
    min-height: 28px;
    border-radius: 7px;
    border: 1px dashed #3a3a5a;
    background: transparent;
    color: #6b6b8a;
    font-size: 11px;
    cursor: pointer;
  }
  .edit-btn:hover { color: #a09ac0; border-color: #4a4a6a; }

  .thumbs { display: flex; gap: 4px; }

  .thumb-btn {
    width: 28px;
    height: 28px;
    border-radius: 7px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    font-size: 13px;
    cursor: pointer;
  }
  .thumb-btn:hover { border-color: #4a3a7a; }

  .opened-at {
    margin-left: auto;
    font-size: 10px;
    color: #4a4a6a;
  }

  .edit-prompt-row {
    display: flex;
    gap: 6px;
  }

  .edit-input {
    flex: 1;
    min-width: 0;
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 7px;
    color: #e0dff5;
    font: inherit;
    font-size: 12px;
    padding: 6px 9px;
  }
  .edit-input:focus { outline: none; border-color: #7c3aed; }

  .edit-submit, .edit-cancel {
    padding: 5px 10px;
    min-height: 28px;
    border-radius: 7px;
    border: none;
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
  }
  .edit-submit { background: #7c3aed; color: #fff; }
  .edit-submit:disabled { opacity: 0.5; cursor: default; }
  .edit-cancel { background: #2a2a45; color: #ddd8f5; }
</style>
