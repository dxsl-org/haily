<script lang="ts">
  // Mirrors `+page.svelte`'s chunk-accumulation pattern (desktop), trimmed to what mobile v1
  // needs: no Cockpit/Settings/Safety-tab undo list (YAGNI — those are desktop-only surfaces
  // for now). `onChunk`/`resolveApproval`/`cancelTurn` are the SAME desktop Tauri command/event
  // names (`$lib/tauri.ts`) — `src-tauri-mobile` registers its own handlers under those exact
  // names so this component and `ApprovalModal`/`ProactivePanel` need no fork. `send_message`
  // (mint-server-side) stays reserved for `ProactivePanel`'s fire-and-forget replies; THIS
  // component's own send button uses `mobileSendMessage` (caller-supplied session id) so it can
  // pre-register `sessionIndex` before the command resolves (see `send()` below).
  import { onMount } from 'svelte';
  import { onChunk, cancelTurn, type ChunkPayload, type PendingApproval } from '$lib/tauri';
  import {
    mobileSendMessage,
    mobileFetchSession,
    onResyncNeeded,
    onApprovalDenied,
  } from './mobile-tauri';
  import ApprovalModal from '$lib/components/ApprovalModal.svelte';
  import ProactivePanel from '$lib/components/ProactivePanel.svelte';

  type Role = 'user' | 'assistant' | 'system';
  interface Message {
    id: string;
    role: Role;
    content: string;
    pending: boolean;
  }

  const NIL_UUID = '00000000-0000-0000-0000-000000000000';
  const KNOWN_ROLES: Role[] = ['user', 'assistant', 'system'];

  let pendingApproval = $state<PendingApproval | null>(null);
  let activeSession = $state<string | null>(null);
  let stopping = $state(false);
  let deniedNotice = $state<string | null>(null);
  let messages = $state<Message[]>([
    { id: 'welcome', role: 'system', content: 'Connected to Haily.', pending: false },
  ]);
  let input = $state('');
  let sending = $state(false);
  let bottomAnchor: HTMLDivElement;

  const sessionIndex = new Map<string, number>();

  onMount(() => {
    const unlistenChunk = onChunk(({ session_id, chunk }: ChunkPayload) => {
      if (chunk.type === 'ToolApprovalRequest') {
        pendingApproval = {
          sessionId: session_id,
          approvalId: chunk.data.approval_id,
          tool: chunk.data.tool,
          args: chunk.data.args,
          origin: chunk.data.origin,
          reversible: chunk.data.reversible,
        };
        return;
      }

      let idx: number | undefined;
      if (session_id === NIL_UUID) {
        messages.push({ id: crypto.randomUUID(), role: 'system', content: '', pending: true });
        idx = messages.length - 1;
      } else {
        idx = sessionIndex.get(session_id);
      }
      if (idx === undefined) return;

      if (chunk.type === 'Text') {
        messages[idx].content += chunk.data;
      } else if (chunk.type === 'Error') {
        messages[idx].content += `\n⚠️ ${chunk.data}`;
      } else if (chunk.type === 'Complete') {
        messages[idx].pending = false;
        sessionIndex.delete(session_id);
        if (activeSession === session_id) {
          activeSession = null;
          stopping = false;
        }
      }
      scrollToBottom();
    });

    // M7/C4: the bridge has no notion of "which session is open" — that's this component's own
    // state, so IT drives the re-fetch. Only the currently-active turn is worth resyncing;
    // anything else the user was looking at is already what it is (v1 has no persisted history
    // to reconcile against, M5 — see the phase's Deviation Log for the scope this covers).
    const unlistenResync = onResyncNeeded(() => {
      if (!activeSession) return;
      const sid = activeSession;
      mobileFetchSession(sid)
        .then((snapshot) => {
          messages = snapshot.transcript.map((entry) => ({
            id: crypto.randomUUID(),
            role: KNOWN_ROLES.includes(entry.role as Role) ? (entry.role as Role) : 'system',
            content: entry.content,
            pending: false,
          }));
          // The live stream reference this session's `sessionIndex` entry pointed at is gone
          // (the snapshot REPLACES the view wholesale, §6.3) — a new turn starts fresh rather
          // than trying to reconcile position-in-array against the just-replaced list.
          sessionIndex.delete(sid);
          activeSession = null;
          scrollToBottom();
        })
        .catch((e) => console.error('mobileFetchSession (resync) failed', e));
    });

    // M1: the user tapped Approve but the biometric check failed/was cancelled — the server
    // will honor this as a deny per `mobile_approval_policy`, but `ApprovalModal` (shared,
    // unmodified) already closed its dialog with no notion of that outcome. Surface it here so
    // the UI doesn't silently imply the action went through.
    const unlistenDenied = onApprovalDenied(() => {
      deniedNotice = 'Action denied — biometric check failed';
      setTimeout(() => {
        deniedNotice = null;
      }, 4000);
    });

    return () => {
      unlistenChunk.then((fn) => fn());
      unlistenResync.then((fn) => fn());
      unlistenDenied.then((fn) => fn());
    };
  });

  async function send() {
    const text = input.trim();
    if (!text || sending || pendingApproval || activeSession) return;
    input = '';
    sending = true;

    // Mint the session id HERE (client-side) and register `sessionIndex` BEFORE the command
    // even goes out — a `haily-chunk` event for this session can arrive over its own IPC
    // channel before `mobileSendMessage`'s return value does (no ordering guarantee between the
    // two), so registering only after `await` would risk dropping the earliest chunk(s).
    const sessionId = crypto.randomUUID();
    messages.push({ id: crypto.randomUUID(), role: 'user', content: text, pending: false });
    const assistantIdx = messages.length;
    messages.push({ id: crypto.randomUUID(), role: 'assistant', content: '', pending: true });
    sessionIndex.set(sessionId, assistantIdx);
    activeSession = sessionId;
    scrollToBottom();

    try {
      await mobileSendMessage(sessionId, text);
    } catch (e) {
      messages[assistantIdx].content = `⚠️ Connection error: ${e}`;
      messages[assistantIdx].pending = false;
      sessionIndex.delete(sessionId);
      activeSession = null;
    } finally {
      sending = false;
    }
  }

  /** Mirrors `+page.svelte`'s (desktop) `stop()` — fires cancellation and tracks the transient
   * "stopping…" label; the backend still emits a terminal `Complete`/`Error` chunk afterward, so
   * the bubble closes out via the normal chunk-handling path above. */
  async function stop() {
    if (!activeSession || stopping) return;
    stopping = true;
    try {
      await cancelTurn(activeSession);
    } catch (e) {
      console.error('cancelTurn failed', e);
      stopping = false;
    }
  }

  function scrollToBottom() {
    requestAnimationFrame(() => bottomAnchor?.scrollIntoView({ behavior: 'smooth' }));
  }
</script>

<ApprovalModal bind:pending={pendingApproval} />
<ProactivePanel />

{#if deniedNotice}
  <div class="denied-notice" role="alert">⚠️ {deniedNotice}</div>
{/if}

<div class="messages">
  {#each messages as msg (msg.id)}
    <div class="bubble {msg.role}" class:pending={msg.pending}>
      <span class="text">{msg.content}</span>
      {#if msg.pending}<span class="cursor">▋</span>{/if}
    </div>
  {/each}
  <div bind:this={bottomAnchor}></div>
</div>

<div class="input-area">
  <textarea
    bind:value={input}
    onkeydown={(e) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        send();
      }
    }}
    placeholder={pendingApproval ? 'Waiting for your approval…' : 'Message Haily…'}
    rows="1"
    disabled={sending || pendingApproval !== null || activeSession !== null}
  ></textarea>
  {#if activeSession}
    <button class="stop" onclick={stop} disabled={stopping} title="Stop" aria-label="Stop">
      {stopping ? '…' : '■'}
    </button>
  {:else}
    <button onclick={send} disabled={sending || !input.trim() || pendingApproval !== null}>
      ↑
    </button>
  {/if}
</div>

<style>
  .denied-notice {
    padding: 8px 14px;
    font-size: 12px;
    background: #2a1f0f;
    color: #fbbf24;
    border-bottom: 1px solid #3a2a5a;
  }
  .messages {
    flex: 1;
    overflow-y: auto;
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .bubble {
    max-width: 85%;
    padding: 9px 13px;
    border-radius: 14px;
    line-height: 1.5;
    white-space: pre-wrap;
    word-break: break-word;
    font-size: 14px;
  }
  .bubble.user { background: #7c3aed; color: #fff; align-self: flex-end; }
  .bubble.assistant { background: #1a1a2e; color: #ddd8f5; align-self: flex-start; }
  .bubble.system { background: transparent; border: 1px solid #2a2a45; color: #8884aa; align-self: center; font-size: 12px; }
  .cursor { animation: blink 1s step-end infinite; color: #c084fc; }
  @keyframes blink { 50% { opacity: 0; } }

  .input-area {
    display: flex;
    gap: 8px;
    padding: 10px 12px;
    border-top: 1px solid #1e1e2e;
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
  }
  button {
    width: 40px;
    border-radius: 10px;
    border: none;
    background: #7c3aed;
    color: #fff;
    font-size: 18px;
    cursor: pointer;
  }
  button:disabled { opacity: 0.4; cursor: default; }
  button.stop { background: #3a1f2e; color: #f87171; font-size: 15px; }
</style>
