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
  // Pure renderer of `+page.svelte`'s lifted `messages` state — the `haily-chunk`
  // subscription that fills it lives at the page (see `+page.svelte`'s `onMount`), NOT
  // here, because this component only mounts while `route === 'chat'`: a listener owned
  // by a route-gated component would drop every chunk that arrives while the user is on
  // another destination (missed `Complete` wedges `activeSession` forever, a missed
  // `ToolApprovalRequest` silently stalls to its 120s deny). This file stays route-scoped
  // on purpose; only the subscription had to move.
  import { sendMessage } from '$lib/tauri';
  import ChatBubble from './ChatBubble.svelte';

  interface Props {
    /** Same reactive array `ChatInput` pushes new turns onto and `+page.svelte`'s
     * listener mutates in place — passed by reference, never reassigned here. */
    messages: Message[];
    bottomAnchor: HTMLDivElement | undefined;
  }

  let { messages, bottomAnchor = $bindable() }: Props = $props();

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
