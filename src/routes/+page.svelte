<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import Settings from '$lib/components/Settings.svelte';
  import ApprovalModal from '$lib/components/ApprovalModal.svelte';
  import { cancelTurn } from '$lib/tauri';
  import type { ChunkPayload, PendingApproval } from '$lib/tauri';

  type Role = 'user' | 'assistant' | 'system';

  interface Message {
    id: string;
    role: Role;
    content: string;
    pending: boolean;
  }

  const NIL_UUID = '00000000-0000-0000-0000-000000000000';

  let settingsOpen = $state(false);
  let pendingApproval = $state<PendingApproval | null>(null);
  // The session_id of the turn currently streaming, or null when idle. Drives the
  // send/Stop button swap — set when `send_message` returns, cleared when that
  // session's `Complete`/`Error` chunk arrives (mirrors `sessionIndex`'s lifecycle).
  let activeSession = $state<string | null>(null);
  let stopping = $state(false);

  let messages = $state<Message[]>([
    {
      id: 'welcome',
      role: 'system',
      content: 'Xin chào! Tôi là Haily 💜 Hỏi tôi bất cứ điều gì.',
      pending: false,
    },
  ]);
  let input = $state('');
  let sending = $state(false);
  let bottomAnchor: HTMLDivElement;
  let textarea: HTMLTextAreaElement;

  // session_id → index in messages[] of the pending assistant bubble
  const sessionIndex = new Map<string, number>();

  onMount(() => {
    const unlistenPromise = listen<ChunkPayload>('haily-chunk', ({ payload }) => {
      const { session_id, chunk } = payload;

      // A pending approval blocks the backend turn regardless of which bubble it's
      // tied to, so it's handled before the bubble lookup (which requires an
      // already-tracked session index).
      if (chunk.type === 'ToolApprovalRequest') {
        pendingApproval = {
          sessionId: session_id,
          approvalId: chunk.data.approval_id,
          tool: chunk.data.tool,
          args: chunk.data.args,
        };
        return;
      }

      // Determine which message bubble to update
      let idx: number | undefined;
      if (session_id === NIL_UUID) {
        // Proactive notification — create a new system bubble
        messages.push({ id: crypto.randomUUID(), role: 'system', content: '', pending: true });
        idx = messages.length - 1;
      } else {
        idx = sessionIndex.get(session_id);
      }

      if (idx === undefined) return;

      if (chunk.type === 'Text') {
        messages[idx].content += chunk.data;
      } else if (chunk.type === 'Error') {
        // Append distinctly rather than silently folding into the running text —
        // the GUI doesn't buffer-then-flush like Telegram, but a bare append would
        // still visually run the error into whatever partial answer streamed first.
        messages[idx].content += `\n⚠️ ${chunk.data}`;
      } else if (chunk.type === 'Complete') {
        messages[idx].pending = false;
        sessionIndex.delete(session_id);
        if (activeSession === session_id) {
          activeSession = null;
          stopping = false;
        }
      }
      // ToolResult chunks are status-only (✓/✗ tool name) — the CLI/Telegram
      // adapters surface them inline; the GUI has no dedicated status line for them
      // yet and intentionally ignores them here rather than half-rendering.

      scrollToBottom();
    });

    textarea?.focus();
    return () => { unlistenPromise.then(fn => fn()); };
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

    messages.push({ id: crypto.randomUUID(), role: 'user', content: text, pending: false });

    const assistantIdx = messages.length;
    messages.push({ id: crypto.randomUUID(), role: 'assistant', content: '', pending: true });
    scrollToBottom();

    try {
      const sessionId = await invoke<string>('send_message', { message: text });
      sessionIndex.set(sessionId, assistantIdx);
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
   * chunk after cancellation, so the bubble closes out via the normal chunk-handling
   * path above rather than being mutated here — this only fires the cancellation and
   * tracks the transient "stopping…" state for the button label. */
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

  function scrollToBottom() {
    requestAnimationFrame(() => bottomAnchor?.scrollIntoView({ behavior: 'smooth' }));
  }
</script>

<div class="app">
  <header>
    <span class="logo">Haily</span>
    <span class="subtitle">trợ lý ảo</span>
    <button class="gear" onclick={() => settingsOpen = true} aria-label="Cài đặt" title="Cài đặt">
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="12" cy="12" r="3"/>
        <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>
      </svg>
    </button>
  </header>

  <Settings bind:open={settingsOpen} />
  <ApprovalModal bind:pending={pendingApproval} />

  <div class="messages">
    {#each messages as msg (msg.id)}
      <div class="bubble {msg.role}" class:pending={msg.pending}>
        {#if msg.role === 'assistant' && msg.pending && !msg.content}
          <span class="typing"><span></span><span></span><span></span></span>
        {:else}
          <span class="text">{msg.content}</span>
          {#if msg.pending}
            <span class="cursor">▋</span>
          {/if}
        {/if}
      </div>
    {/each}
    <div bind:this={bottomAnchor}></div>
  </div>

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
</div>

<style>
  :global(*) { box-sizing: border-box; margin: 0; padding: 0; }

  :global(body) {
    background: #0f0f12;
    color: #e0dff5;
    font-family: system-ui, 'Segoe UI', sans-serif;
    font-size: 14px;
    height: 100dvh;
    overflow: hidden;
  }

  .app {
    display: flex;
    flex-direction: column;
    height: 100dvh;
  }

  header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px 16px;
    border-bottom: 1px solid #1e1e2e;
    flex-shrink: 0;
  }

  .gear {
    margin-left: auto;
    width: 30px;
    height: 30px;
    border: none;
    border-radius: 8px;
    background: transparent;
    color: #4a4a6a;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: color 0.15s, background 0.15s;
  }
  .gear:hover { color: #c084fc; background: #1a1a2e; }

  .logo {
    font-weight: 700;
    font-size: 16px;
    color: #c084fc;
    letter-spacing: -0.3px;
  }

  .subtitle {
    font-size: 12px;
    color: #6b6b8a;
    align-self: baseline;
  }

  .messages {
    flex: 1;
    overflow-y: auto;
    padding: 16px 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    scroll-behavior: smooth;
  }

  .messages::-webkit-scrollbar { width: 4px; }
  .messages::-webkit-scrollbar-thumb { background: #2e2e45; border-radius: 2px; }

  .bubble {
    max-width: 80%;
    padding: 9px 13px;
    border-radius: 14px;
    line-height: 1.55;
    white-space: pre-wrap;
    word-break: break-word;
    font-size: 14px;
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
