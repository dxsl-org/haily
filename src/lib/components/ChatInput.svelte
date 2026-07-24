<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { cancelTurn } from '$lib/tauri';
  import type { PendingApproval } from '$lib/tauri';
  import type { Message } from './ChatStream.svelte';
  import SlashPalette from './SlashPalette.svelte';
  import { spliceCommand } from '$lib/slash-insert';
  import { createSlashPaletteState } from '$lib/chat-palette-state.svelte';

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
  // Must be reactive (`$state`, not a plain `let`): passed as `SlashPalette`'s `anchorEl`
  // prop, which needs to see the real element once `bind:this` assigns it post-mount —
  // a plain `let` would leave the child holding the pre-mount `undefined` forever.
  let plusButton = $state<HTMLButtonElement | undefined>(undefined);

  // Slash palette (P03, D6): both entry paths ("/" inline trigger + ＋ button) read
  // from the same `palette` state so behavior can't diverge — see `chat-palette-state`.
  const palette = createSlashPaletteState(() => input);

  /** Selecting a row inserts `/<name> ` and returns focus to the input — never sends. */
  function insertCommand(name: string) {
    const start = palette.usingPlus ? (textarea?.selectionStart ?? input.length) : 0;
    const end = palette.usingPlus ? (textarea?.selectionEnd ?? input.length) : input.length;
    const spliced = spliceCommand(input, name, start, end);
    input = spliced.text;
    palette.close();
    requestAnimationFrame(() => {
      textarea?.focus();
      textarea?.setSelectionRange(spliced.caret, spliced.caret);
      autoResize();
    });
  }

  onMount(() => {
    textarea?.focus();
  });

  async function send() {
    const text = input.trim();
    // Block while an approval is pending (would orphan it to a silent 120s timeout-deny)
    // or a turn is already streaming (single-turn-at-a-time GUI; a second send would
    // overwrite the Stop button's target session).
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

  /** Stop the streaming turn. Backend still emits a terminal `Complete` chunk after
   * cancellation, so the bubble closes via `ChatStream`'s normal handling, not here. */
  async function stop() {
    if (!activeSession || stopping) return;
    stopping = true;
    try {
      await cancelTurn(activeSession);
    } catch (e) {
      // Best-effort: a genuine invoke error (not the "no turn found" false result)
      // leaves the bubble streaming rather than silently losing the failure.
      console.error('cancelTurn failed', e);
      stopping = false;
    }
  }

  // ↑/↓/Enter/Tab/Esc are owned by `SlashPalette` (capture-phase listener) while it's
  // open — this guard stops the same keystroke from ALSO sending the message here.
  function onKeydown(e: KeyboardEvent) {
    if (palette.open && ['ArrowDown', 'ArrowUp', 'Enter', 'Tab', 'Escape'].includes(e.key)) return;
    // Never intercept Enter mid-IME-composition (Vietnamese Telex/VNI, CJK, …) — it commits
    // the composed text, it is not a request to send.
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      send();
    }
  }

  function onInput() {
    palette.onTyped();
    autoResize();
  }

  function autoResize() {
    if (!textarea) return;
    textarea.style.height = 'auto';
    textarea.style.height = Math.min(textarea.scrollHeight, 160) + 'px';
  }

  // Shared by the textarea and the ＋ button — both must go inert together while a turn
  // is in flight or an approval is pending (M03 review: the ＋ button previously stayed
  // clickable while the textarea it edits was disabled).
  const inputDisabled = $derived(sending || pendingApproval !== null || activeSession !== null);
</script>

<div class="input-area">
  <SlashPalette
    open={palette.open}
    filter={palette.filter}
    onSelect={insertCommand}
    onClose={palette.close}
    anchorEl={plusButton}
  />
  <button
    bind:this={plusButton}
    class="plus"
    onclick={palette.togglePlus}
    disabled={inputDisabled}
    title="Danh sách lệnh"
    aria-label="Danh sách lệnh"
    aria-expanded={palette.usingPlus}
  >＋</button>
  <textarea
    bind:this={textarea}
    bind:value={input}
    onkeydown={onKeydown}
    oninput={onInput}
    placeholder={pendingApproval ? 'Đang chờ bạn duyệt một hành động…' : 'Nhắn tin với Haily… (Enter để gửi, Shift+Enter xuống dòng, / để xem lệnh)'}
    rows="1"
    disabled={inputDisabled}
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
    position: relative;
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

  button.plus { background: #2a2a45; color: #c084fc; font-size: 20px; font-weight: 700; }
  button.plus:hover { background: #35355a; }
  button.plus[aria-expanded='true'] { background: #4c1d95; color: #fff; }
</style>
