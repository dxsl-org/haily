<script lang="ts">
  import type { Message } from './ChatStream.svelte';

  interface Props {
    msg: Message;
    /** journalId of the undo currently in flight, or `null` — disables just that button
     * (see `ChatStream`'s `requestUndo`, the sole caller of `onUndo`). */
    undoingId: string | null;
    onUndo: (journalId: string) => void;
  }

  let { msg, undoingId, onUndo }: Props = $props();
</script>

<div class="bubble {msg.role}" class:pending={msg.pending}>
  {#if msg.role === 'assistant' && msg.pending && !msg.content}
    <span class="typing"><span></span><span></span><span></span></span>
  {:else}
    <span class="text">{msg.content}</span>
    {#if msg.pending}
      <span class="cursor">▋</span>
    {/if}
  {/if}
  <!-- Gated on `!msg.pending` (the turn's Complete chunk landed) per M4 button gating —
       undo affordances for this turn's writes only appear once nothing else from this
       turn can still land. -->
  {#if !msg.pending && msg.undoable.length > 0}
    <div class="undo-list">
      {#each msg.undoable as action (action.journalId)}
        <button
          class="undo-inline"
          onclick={() => onUndo(action.journalId)}
          disabled={undoingId === action.journalId}
          title={action.verb}
        >
          {undoingId === action.journalId ? 'Đang hoàn tác…' : '↩ Hoàn tác'}
        </button>
      {/each}
    </div>
  {/if}
  <!-- Same `!msg.pending` gate as the undo list: a badge is only meaningful once the
       turn it describes has actually finished. `routing_enabled=false` or a non-assistant
       bubble never sets `badge`, so this renders nothing then. -->
  {#if !msg.pending && msg.badge}
    <div class="turn-badge">{msg.badge}</div>
  {/if}
</div>

<style>
  .bubble {
    max-width: 80%;
    padding: 9px 13px;
    border-radius: 14px;
    line-height: 1.55;
    white-space: pre-wrap;
    word-break: break-word;
    font-size: 14px;
  }

  .undo-list {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-top: 8px;
  }

  /* min-height 44px meets the WCAG 2.1 AA (2.5.5) minimum touch target size. */
  .undo-inline {
    padding: 8px 12px;
    min-height: 44px;
    border: 1px solid #3a2a5a;
    border-radius: 999px;
    background: #1e1638;
    color: #c084fc;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
    white-space: normal;
    transition: border-color 0.15s, background 0.15s;
  }

  .undo-inline:hover:not(:disabled) { border-color: #7c3aed; background: #271a4a; }
  .undo-inline:disabled { opacity: 0.5; cursor: default; }

  .turn-badge {
    margin-top: 6px;
    font-size: 11px;
    color: #7c7c9a;
    opacity: 0.8;
  }

  .bubble.user {
    background: #7c3aed;
    color: #f3f0ff;
    align-self: flex-end;
    border-bottom-right-radius: 4px;
  }

  .bubble.assistant {
    background: #1a1a2e;
    color: #ddd8f5;
    align-self: flex-start;
    border-bottom-left-radius: 4px;
    min-width: 40px;
    min-height: 36px;
  }

  .bubble.system {
    background: transparent;
    border: 1px solid #2a2a45;
    color: #8884aa;
    align-self: center;
    font-size: 12px;
    border-radius: 8px;
    max-width: 90%;
  }

  .cursor {
    display: inline-block;
    animation: blink 1s step-end infinite;
    opacity: 0.8;
    color: #c084fc;
    margin-left: 1px;
  }

  @keyframes blink {
    50% { opacity: 0; }
  }

  .typing {
    display: inline-flex;
    gap: 4px;
    align-items: center;
    padding: 4px 0;
  }

  .typing span {
    width: 6px;
    height: 6px;
    background: #6b6b9a;
    border-radius: 50%;
    animation: bounce 1.2s ease-in-out infinite;
  }

  .typing span:nth-child(2) { animation-delay: 0.15s; }
  .typing span:nth-child(3) { animation-delay: 0.3s; }

  @keyframes bounce {
    0%, 60%, 100% { transform: translateY(0); }
    30% { transform: translateY(-5px); }
  }
</style>
