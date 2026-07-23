<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { cancelTurn } from '$lib/tauri';
  import type { PendingApproval } from '$lib/tauri';
  import type { Message } from './ChatStream.svelte';

  interface Props {
    /** Same reactive array `ChatStream` renders — mutated in place (push), never
     * reassigned here. */
    messages: Message[];
    /** session_id → index in `messages[]`; this component is the sole writer of new
     * entries, `ChatStream`'s chunk listener reads/deletes them. */
    sessionIndex: Map<string, number>;
    /** Every session id started this GUI instance, oldest first — read by `Settings`'
     * Safety tab via `+page.svelte`'s `getSessionIds` getter. */
    seenSessionIds: string[];
    pendingApproval: PendingApproval | null;
    activeSession: string | null;
    stopping: boolean;
    scrollToBottom: () => void;
  }

  let {
    messages,
    sessionIndex,
    seenSessionIds,
    pendingApproval,
    activeSession = $bindable(null),
    stopping = $bindable(false),
    scrollToBottom,
  }: Props = $props();

  let input = $state('');
  let sending = $state(false);
  let textarea: HTMLTextAreaElement;

  onMount(() => {
    textarea?.focus();
  });

  async function send() {
    const text = input.trim();
    // Block new turns while a tool approval is pending: the backend turn is paused
    // waiting on the modal, and starting a second turn would overwrite the pending
    // approval state (orphaning the first request to a silent 120s timeout-deny).
    // Also block while a turn is already streaming (`activeSession` set) — the GUI
    // is single-turn-at-a-time; a second concurrent send would overwrite the Stop
    // button's target with no way back to the first turn's session id.
    if (!text || sending || pendingApproval || activeSession) return;

    input = '';
    sending = true;
    autoResize();

    messages.push({ id: crypto.randomUUID(), role: 'user', content: text, pending: false, undoable: [], badge: null });

    const assistantIdx = messages.length;
    messages.push({ id: crypto.randomUUID(), role: 'assistant', content: '', pending: true, undoable: [], badge: null });
    scrollToBottom();

    try {
      const sessionId = await invoke<string>('send_message', { message: text });
      sessionIndex.set(sessionId, assistantIdx);
      seenSessionIds.push(sessionId);
      activeSession = sessionId;
    } catch (e) {
      messages[assistantIdx].content = `⚠️ Lỗi kết nối: ${e}`;
      messages[assistantIdx].pending = false;
    } finally {
      sending = false;
      textarea?.focus();
    }
  }

  /** Stop the currently streaming turn. Backend still emits a terminal `Complete`
   * chunk after cancellation, so the bubble closes out via `ChatStream`'s normal
   * chunk-handling path rather than being mutated here — this only fires the
   * cancellation and tracks the transient "stopping…" state for the button label. */
  async function stop() {
    if (!activeSession || stopping) return;
    stopping = true;
    try {
      await cancelTurn(activeSession);
    } catch (e) {
      // Cancellation is best-effort from the UI's perspective — if the IPC call
      // itself fails (not "no turn found", which resolves false, but a genuine
      // invoke error), leave the bubble streaming rather than silently losing the
      // failure; the console log gives a debugging trail without a modal.
      console.error('cancelTurn failed', e);
      stopping = false;
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }

  function autoResize() {
    if (!textarea) return;
    textarea.style.height = 'auto';
    textarea.style.height = Math.min(textarea.scrollHeight, 160) + 'px';
  }
</script>

<div class="input-area">
  <textarea
    bind:this={textarea}
    bind:value={input}
    onkeydown={onKeydown}
    oninput={autoResize}
    placeholder={pendingApproval ? 'Đang chờ bạn duyệt một hành động…' : 'Nhắn tin với Haily… (Enter để gửi, Shift+Enter xuống dòng)'}
    rows="1"
    disabled={sending || pendingApproval !== null || activeSession !== null}
  ></textarea>
  {#if activeSession}
    <button class="stop" onclick={stop} disabled={stopping} title="Dừng phản hồi" aria-label="Dừng phản hồi">
      {stopping ? '…' : '■'}
    </button>
  {:else}
    <button onclick={send} disabled={sending || !input.trim() || pendingApproval !== null}>
      {sending ? '…' : '↑'}
    </button>
  {/if}
</div>

<style>
  .input-area {
    display: flex;
    gap: 8px;
    padding: 10px 12px;
    border-top: 1px solid #1e1e2e;
    align-items: flex-end;
    flex-shrink: 0;
  }

  textarea {
    flex: 1;
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 12px;
    color: #e0dff5;
    font: inherit;
    padding: 9px 12px;
    resize: none;
    outline: none;
    line-height: 1.5;
    min-height: 40px;
    max-height: 160px;
    transition: border-color 0.15s;
  }

  textarea:focus { border-color: #7c3aed; }

  textarea::placeholder { color: #4a4a6a; }

  textarea:disabled { opacity: 0.5; }

  button {
    width: 40px;
    height: 40px;
    border-radius: 10px;
    border: none;
    background: #7c3aed;
    color: #fff;
    font-size: 18px;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    transition: background 0.15s, opacity 0.15s;
  }

  button:hover:not(:disabled) { background: #8b5cf6; }
  button:disabled { opacity: 0.4; cursor: default; }

  button.stop { background: #3a1f2e; color: #f87171; font-size: 15px; }
  button.stop:hover:not(:disabled) { background: #4a2436; }
</style>
