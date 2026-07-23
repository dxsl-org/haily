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
  // The run-event subscription below is DIFFERENT (P04): it owns its own `onRunEvents`
  // listener, scoped to this component's mount lifetime same as the former
  // `RunTimeline.svelte`. This IS route-gated on purpose — a run's progress card is only
  // ever shown "in the chat flow of the session that launched it" (phase-04 spec), and
  // the accepted interim gap (no persisted reconcile until P05/P07) means a run's events
  // that arrive while the user is off the chat route are missed, same tradeoff
  // `RunTimeline` always had.
  import { onMount, onDestroy } from 'svelte';
  import { sendMessage, onRunEvents, type PendingApproval, type RunEventPayload } from '$lib/tauri';
  import { applyRunEvent, orderedJobs, type Job } from '$lib/run-events';
  import ChatBubble from './ChatBubble.svelte';
  import RunProgressCard from './RunProgressCard.svelte';
  import ApprovalQueue from './ApprovalQueue.svelte';

  interface Props {
    /** Same reactive array `ChatInput` pushes new turns onto and `+page.svelte`'s
     * listener mutates in place — passed by reference, never reassigned here. */
    messages: Message[];
    bottomAnchor: HTMLDivElement | undefined;
    /** session_id → index in `messages[]`, read-only here — used to anchor a run's
     * progress card to the message that launched it (P04). Never mutated by this
     * component; `ChatInput`/`+page.svelte`'s chunk listener own writes/deletes. */
    sessionIndex: Map<string, number>;
    /** Shared approval queue (P04, D6) — `+page.svelte` owns the array (pushed to from
     * the `haily-chunk` listener); passed by reference like `messages`. */
    approvals: PendingApproval[];
    /** Tells `+page.svelte` an approval was resolved (or found already-stale) here, so it
     * can drop the entry — and, if the same id is still shown in the out-of-session
     * modal, clear that too (see `+page.svelte`'s sync effect). */
    onApprovalResolved: (approvalId: string) => void;
  }

  let { messages, bottomAnchor = $bindable(), sessionIndex, approvals, onApprovalResolved }: Props = $props();

  // Per-run message anchor, captured ONCE the first time this component observes an
  // event for a given run_id (never overwritten after) — `sessionIndex` entries are
  // deleted once that turn's `Complete` chunk lands (often well before a long pipeline
  // run finishes), so reading it live at render time would go stale; capturing at
  // first-sight is the only correct moment.
  const jobAnchors = new Map<string, number>();
  let jobs = $state<Map<string, Job>>(new Map());
  let showAllActive = $state(false);
  let unlistenRuns: (() => void) | undefined;

  onMount(() => {
    const unlistenPromise = onRunEvents(({ session_id, event }: RunEventPayload) => {
      const runId = event.data.run_id;
      if (!jobAnchors.has(runId)) {
        jobAnchors.set(runId, sessionIndex.get(session_id) ?? Math.max(messages.length - 1, 0));
      }
      jobs = applyRunEvent(jobs, session_id, event);
    });
    unlistenRuns = () => { unlistenPromise.then((fn) => fn()); };
  });

  onDestroy(() => unlistenRuns?.());

  const jobList = $derived(orderedJobs(jobs));
  const activeJobs = $derived(jobList.filter((j) => j.status === 'running' || j.status === 'paused'));
  // Groups jobs by their anchor message index so the render loop below can splice a
  // run's card in right after the bubble that launched it.
  const jobsByAnchor = $derived.by(() => {
    const map = new Map<number, Job[]>();
    for (const job of jobList) {
      const anchor = jobAnchors.get(job.runId) ?? Math.max(messages.length - 1, 0);
      const arr = map.get(anchor) ?? [];
      arr.push(job);
      map.set(anchor, arr);
    }
    return map;
  });

  // Chat-overload mitigation (phase-04 risk assessment): a finished job always renders
  // its own card (it's a result, not noise), but once MORE THAN ONE run is active at
  // once, all active cards collapse behind a single "N running" chip until the user
  // opts to expand — mirrors the Claude-Desktop background-tasks pattern.
  function showCard(job: Job): boolean {
    if (job.status !== 'running' && job.status !== 'paused') return true;
    return activeJobs.length <= 1 || showAllActive;
  }

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
      {#each jobsByAnchor.get(i) ?? [] as job (job.runId)}
        {#if showCard(job)}
          <RunProgressCard {job} />
        {/if}
      {/each}
    {/each}

    {#if activeJobs.length > 1 && !showAllActive}
      <button class="active-chip" onclick={() => (showAllActive = true)}>
        {activeJobs.length} tác vụ đang chạy nền ▸
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
