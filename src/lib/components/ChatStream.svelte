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
  //
  // `jobsState` (P04 review-fix MED-1) is the SAME reason, corrected: it used to be a
  // component-local `onRunEvents` subscription here, which tore down on every route
  // switch away from chat and re-created each job from scratch on return (elapsed reset
  // to ~00:00, retry/escalation counts undercounted — only post-remount events folded).
  // `+page.svelte` now owns one instance for the app's lifetime; this component only
  // reads from it, exactly like it reads `messages`.
  import { sendMessage, type PendingApproval } from '$lib/tauri';
  import type { RunJobsState } from '$lib/run-jobs-state.svelte';
  import ChatBubble from './ChatBubble.svelte';
  import RunProgressCard from './RunProgressCard.svelte';
  import ApprovalQueue from './ApprovalQueue.svelte';

  interface Props {
    /** Same reactive array `ChatInput` pushes new turns onto and `+page.svelte`'s
     * listener mutates in place — passed by reference, never reassigned here. */
    messages: Message[];
    bottomAnchor: HTMLDivElement | undefined;
    /** Page-lifetime pipeline-run job state — read-only here, `+page.svelte` is the sole
     * writer via its own `onRunEvents` subscription (see `run-jobs-state.svelte.ts`). */
    jobsState: RunJobsState;
    /** Shared approval queue (P04, D6) — `+page.svelte` owns the array (pushed to from
     * the `haily-chunk` listener); passed by reference like `messages`. */
    approvals: PendingApproval[];
    /** Tells `+page.svelte` an approval was resolved (or found already-stale) here, so it
     * can drop the entry — and, if the same id is still shown in the out-of-session
     * modal, clear that too (see `+page.svelte`'s sync effect). */
    onApprovalResolved: (approvalId: string) => void;
    /** Navigates to the Runs screen's drill-in for a run (Unified Chat UI phase 7, D6) —
     * threaded straight through to `RunProgressCard`'s "Chi tiết →". `undefined` leaves that
     * affordance disabled, mirroring its pre-P07 inert state. */
    onOpenRun?: (runId: string) => void;
  }

  let { messages, bottomAnchor = $bindable(), jobsState, approvals, onApprovalResolved, onOpenRun }: Props = $props();

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

<div class="chat-stream">
  <!-- Pinned above the scrolling message list (not inside `.messages`) so the queue
       stays visible regardless of scroll position — the "single shared queue with a
       header badge" the phase spec calls for. -->
  <ApprovalQueue {approvals} onResolved={onApprovalResolved} />

  <div class="messages">
    {#each messages as msg, i (msg.id)}
      <ChatBubble {msg} undoingId={undoing} onUndo={requestUndo} />
      {#each jobsState.jobsByAnchor.get(i) ?? [] as job (job.runId)}
        {#if jobsState.showCard(job)}
          <RunProgressCard {job} {onOpenRun} />
        {/if}
      {/each}
    {/each}

    <!-- Fallback for a run whose anchor doesn't land inside the current message list
         (review fix LOW-6) — otherwise silently unrendered even though the ">N running"
         chip below would still count it. -->
    {#each jobsState.unanchoredJobs as job (job.runId)}
      {#if jobsState.showCard(job)}
        <RunProgressCard {job} {onOpenRun} />
      {/if}
    {/each}

    {#if jobsState.activeJobs.length > 1 && !jobsState.showAllActive}
      <button class="active-chip" onclick={() => jobsState.expandAllActive()}>
        {jobsState.activeJobs.length} tác vụ đang chạy nền ▸
      </button>
    {/if}

    <div bind:this={bottomAnchor}></div>
  </div>
</div>

<style>
  .chat-stream {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
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

  .active-chip {
    align-self: flex-start;
    padding: 6px 12px;
    min-height: 32px;
    border-radius: 999px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #a09ac0;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .active-chip:hover { border-color: #4a3a7a; color: #c084fc; }
</style>
