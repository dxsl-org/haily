<script module lang="ts">
  export type Role = 'user' | 'assistant' | 'system';

  export interface UndoableAction {
    journalId: string;
    verb: string;
  }

  export interface Message {
    id: string;
    role: Role;
    content: string;
    pending: boolean;
    /** Reversible actions completed during this turn, revealed only once `pending` flips
     * false (the turn's `Complete` chunk arrived) — see `UndoableAction`. */
    undoable: UndoableAction[];
    /** "tier · model" (or bare model name for a `None`-tier turn) from a `TurnMeta`
     * chunk — `null` until it arrives (a legacy/routing-disabled turn never sets it, and
     * a `system`/`user` bubble never receives one). Rendered as a footer once the turn
     * completes, mirroring `undoable`'s "arrives mid-stream, shown at Complete" gating. */
    badge: string | null;
  }
</script>

<script lang="ts">
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { sendMessage } from '$lib/tauri';
  import type { ChunkPayload, PendingApproval } from '$lib/tauri';
  import { toolVerb } from '$lib/tool-verbs';
  import ChatBubble from './ChatBubble.svelte';

  const NIL_UUID = '00000000-0000-0000-0000-000000000000';

  interface Props {
    /** Same reactive array `ChatInput` pushes new turns onto — passed by reference from
     * `+page.svelte`, mutated in place (push/index-write), never reassigned here. */
    messages: Message[];
    /** session_id → index in `messages[]` of that turn's assistant bubble; shared with
     * `ChatInput`'s `send()`, which is the only writer of new entries. */
    sessionIndex: Map<string, number>;
    pendingApproval: PendingApproval | null;
    pendingWorkspaceView: { viewId: string; sessionId: string } | null;
    activeSession: string | null;
    stopping: boolean;
    bottomAnchor: HTMLDivElement | undefined;
    scrollToBottom: () => void;
  }

  let {
    messages,
    sessionIndex,
    pendingApproval = $bindable(null),
    pendingWorkspaceView = $bindable(null),
    activeSession = $bindable(null),
    stopping = $bindable(false),
    bottomAnchor = $bindable(),
    scrollToBottom,
  }: Props = $props();

  // Mirrors `SafetyTab.svelte`'s `requestUndo` phrasing exactly so both undo entry points
  // (this inline button and the Safety tab's recent-actions list) produce identical
  // downstream LLM behavior — same sentence in, same `journal_undo` tool call out. This is
  // UI sugar over the normal chat flow, NOT a new write seam: `journal_undo` still prompts
  // for approval (M4 lock, unchanged) and is session-scoped (M1), enforced server-side.
  let undoing = $state<string | null>(null);
  async function requestUndo(journalId: string) {
    if (undoing) return;
    undoing = journalId;
    try {
      await sendMessage(`Undo the action with journal id "${journalId}".`);
    } catch (e) {
      console.error('undo sendMessage failed', e);
    } finally {
      undoing = null;
    }
  }

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
          origin: chunk.data.origin,
          reversible: chunk.data.reversible,
        };
        return;
      }

      // A presented view is a handle only — the bulk `DataView` payload never rides this
      // stream (`WorkspacePane` fetches it separately via `getView`), so this never touches a
      // message bubble, same early-return shape as `ToolApprovalRequest` above.
      if (chunk.type === 'ViewRef') {
        pendingWorkspaceView = { viewId: chunk.data.view_id, sessionId: session_id };
        return;
      }

      // Determine which message bubble to update
      let idx: number | undefined;
      if (session_id === NIL_UUID) {
        // Proactive notification — create a new system bubble
        messages.push({ id: crypto.randomUUID(), role: 'system', content: '', pending: true, undoable: [], badge: null });
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
      } else if (chunk.type === 'ToolResult') {
        // Buffer only — the [Undo] affordance itself is gated to render after `Complete`
        // (M4 button gating: more writes may still land later in the same turn, and
        // offering undo mid-stream would be misleading about what "this turn" did).
        // `journal_id` is non-null only when `reversible` is true AND the write's
        // `post_state_version` had already landed at emit time (M4 ordering) — trust
        // that invariant here rather than re-deriving it client-side.
        const { name, reversible, journal_id } = chunk.data;
        if (reversible && journal_id) {
          messages[idx].undoable.push({ journalId: journal_id, verb: toolVerb(name, '{}') });
        }
      } else if (chunk.type === 'TurnMeta') {
        // Buffer only — rendered as a footer once `Complete` lands below, same gating
        // as `undoable` (more of this turn could still be in flight).
        messages[idx].badge = chunk.data.badge ?? null;
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

    return () => { unlistenPromise.then(fn => fn()); };
  });
</script>

<div class="messages">
  {#each messages as msg (msg.id)}
    <ChatBubble {msg} undoingId={undoing} onUndo={requestUndo} />
  {/each}
  <div bind:this={bottomAnchor}></div>
</div>

<style>
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
</style>
