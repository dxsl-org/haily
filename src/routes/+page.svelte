<script lang="ts">
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import Settings from '$lib/components/Settings.svelte';
  import ApprovalModal from '$lib/components/ApprovalModal.svelte';
  import WorkItemsPanel from '$lib/components/WorkItemsPanel.svelte';
  import ProactivePanel from '$lib/components/ProactivePanel.svelte';
  import IconRail, { type RouteId } from '$lib/components/IconRail.svelte';
  import ChatStream, { type Message } from '$lib/components/ChatStream.svelte';
  import ChatInput from '$lib/components/ChatInput.svelte';
  import RunsScreen from '$lib/components/RunsScreen.svelte';
  import WorkspacesScreen from '$lib/components/WorkspacesScreen.svelte';
  import SkillsScreen from '$lib/components/SkillsScreen.svelte';
  import WorkspacePane from '$lib/components/view/WorkspacePane.svelte';
  import { listApprovals, onRunEvents, type ChunkPayload, type PendingApproval, type RunEventPayload } from '$lib/tauri';
  import { toolVerb } from '$lib/tool-verbs';
  import { createRunJobsState } from '$lib/run-jobs-state.svelte';

  const NIL_UUID = '00000000-0000-0000-0000-000000000000';

  let settingsOpen = $state(false);
  // Left icon rail (Chat/Runs/Workspaces/Skills) replaces the former chat/cockpit
  // toggle — Settings stays a drawer opened by the rail's gear, not a route, so its
  // overlay behavior is unchanged.
  let route = $state<RouteId>('chat');
  // Single latest-wins approval — feeds `ApprovalModal`, the OUT-OF-SESSION fallback
  // (P04, D6): rendered only while `route !== 'chat'`, since the in-session surface is
  // `ChatStream`'s inline `ApprovalQueue` below.
  let pendingApproval = $state<PendingApproval | null>(null);
  // Every approval this GUI window has observed and not yet resolved, oldest first —
  // the shared queue `ChatStream`'s `ApprovalQueue` renders inline. Populated from the
  // SAME chunk as `pendingApproval` below (both structures see every approval; the two
  // surfaces are simply gated to different routes, not to different data — see the sync
  // effect further down for why a resolve on either side clears both).
  let approvalQueue = $state<PendingApproval[]>([]);

  /** Removes one approval from the shared queue — called by `ChatStream` once it (or a
   * stale idempotent no-op) resolves an entry. Also clears `pendingApproval` when it's
   * the same id: `ChatInput` gates sending on `pendingApproval` truthiness alone, so
   * without this an approval resolved via the inline queue would leave the input
   * permanently blocked (only `ApprovalModal`'s own resolve flow nulls it otherwise). */
  function removeFromQueue(approvalId: string) {
    approvalQueue = approvalQueue.filter((a) => a.approvalId !== approvalId);
    if (pendingApproval?.approvalId === approvalId) {
      pendingApproval = null;
    }
  }
  // Set for one tick whenever a `ViewRef` chunk arrives (written by the `haily-chunk`
  // listener below); `WorkspacePane` consumes and clears it via `bind:pendingView`
  // (mirrors `pendingApproval`'s `bind:pending` contract on `ApprovalModal`). The pane
  // coexists with whichever destination is active — it is never the thing that hides a
  // route.
  let pendingWorkspaceView = $state<{ viewId: string; sessionId: string } | null>(null);
  // The session_id of the turn currently streaming, or null when idle. Written by
  // `ChatInput` on send, cleared by the `haily-chunk` listener below when that session's
  // `Complete`/`Error` chunk arrives — shared with `ChatInput` so it can gate/swap its
  // send/stop button regardless of which route is showing.
  let activeSession = $state<string | null>(null);
  // Transient "stop requested" flag — set by `ChatInput`'s stop button, cleared by the
  // `haily-chunk` listener below once the cancelled turn's `Complete` chunk actually lands.
  let stopping = $state(false);
  let bottomAnchor = $state<HTMLDivElement | undefined>(undefined);

  let messages = $state<Message[]>([
    {
      id: 'welcome',
      role: 'system',
      content: 'Xin chào! Tôi là Haily 💜 Hỏi tôi bất cứ điều gì.',
      pending: false,
      undoable: [],
      badge: null,
    },
  ]);

  // session_id → index in messages[] of the pending assistant bubble — written by
  // `ChatInput.send()`, read/cleared by the `haily-chunk` listener below.
  const sessionIndex = new Map<string, number>();

  // Every session id this GUI instance has started, oldest first — each turn mints a
  // fresh UUID (see `ChatInput.send()`), so there is no single "current session" the
  // Safety tab's recent-actions list could scope to. Passed to `Settings` as a getter so
  // it always reads the live array rather than a snapshot taken when the drawer opened.
  const seenSessionIds: string[] = [];
  const getSessionIds = () => seenSessionIds;

  // Page-lifetime pipeline-run job state (P04 review-fix MED-1) — lives here, NOT inside
  // the route-gated `ChatStream`, so switching away from chat and back doesn't tear down
  // the run-event fold and misreport elapsed/retry counts on return. See
  // `run-jobs-state.svelte.ts`'s module doc for the full incident this fixes.
  const jobsState = createRunJobsState(() => sessionIndex, () => messages.length);

  function scrollToBottom() {
    requestAnimationFrame(() => bottomAnchor?.scrollIntoView({ behavior: 'smooth' }));
  }

  // Page-lifetime subscription (NOT `ChatStream`'s — a route-gated component would drop
  // every chunk that arrives while the user is on Runs/Workspaces/Skills, permanently
  // wedging `activeSession`/a pending approval). `route` switches never tear this down;
  // it lives exactly as long as the page did before the icon-rail split.
  onMount(() => {
    const unlistenPromise = listen<ChunkPayload>('haily-chunk', ({ payload }) => {
      const { session_id, chunk } = payload;

      // A pending approval blocks the backend turn regardless of which bubble it's
      // tied to, so it's handled before the bubble lookup (which requires an
      // already-tracked session index).
      if (chunk.type === 'ToolApprovalRequest') {
        const approval: PendingApproval = {
          sessionId: session_id,
          approvalId: chunk.data.approval_id,
          tool: chunk.data.tool,
          args: chunk.data.args,
          origin: chunk.data.origin,
          reversible: chunk.data.reversible,
        };
        pendingApproval = approval;
        if (!approvalQueue.some((a) => a.approvalId === approval.approvalId)) {
          approvalQueue = [...approvalQueue, approval];
        }
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

  // Page-lifetime run-event subscription (P04 review-fix MED-1) — folds into `jobsState`
  // above, NOT a route-gated component, for the same reason the chunk listener above
  // stays at page level: `ChatStream` only mounts while `route === 'chat'`.
  onMount(() => {
    const unlistenPromise = onRunEvents(({ session_id, event }: RunEventPayload) => {
      jobsState.ingest(session_id, event);
    });
    return () => { unlistenPromise.then((fn) => fn()); };
  });

  // Prunes the shared approval queue (and the out-of-session modal) against the
  // backend's own pending set (P04 review-fix MED-2): there is no `ApprovalResolved`/
  // `ApprovalExpired` chunk, so a server-side 120s auto-deny would otherwise leave a
  // stale entry — and an inflated header badge — sitting client-side until the user
  // happens to click it (idempotent no-op self-heal). Runs on a 30s interval WHILE the
  // queue is non-empty (torn down the instant it drains, restarted the instant it's
  // non-empty again) and once immediately on that transition, covering both "on an
  // interval" and "on queue-open" per the review's fix guidance.
  $effect(() => {
    if (approvalQueue.length === 0) return;
    reconcileApprovals();
    const handle = setInterval(reconcileApprovals, 30_000);
    return () => clearInterval(handle);
  });

  async function reconcileApprovals() {
    try {
      const live = await listApprovals();
      const liveIds = new Set(live.map((a) => a.approval_id));
      approvalQueue = approvalQueue.filter((a) => liveIds.has(a.approvalId));
      if (pendingApproval && !liveIds.has(pendingApproval.approvalId)) {
        pendingApproval = null;
      }
    } catch (e) {
      console.error('listApprovals reconcile failed', e);
    }
  }

  // Keeps the two approval surfaces (out-of-session modal, in-session inline queue) from
  // showing a stale entry for something already decided on the OTHER surface. Resolving
  // via `ApprovalModal` only nulls `pendingApproval` (that component's own contract,
  // unowned by this phase) — this effect notices that null transition and drops the same
  // id from `approvalQueue` too. The reverse direction (queue resolves first) is handled
  // by `removeFromQueue` below, called from `ChatStream`'s `onApprovalResolved`.
  let lastModalApprovalId: string | null = null;
  $effect(() => {
    if (pendingApproval) {
      lastModalApprovalId = pendingApproval.approvalId;
    } else if (lastModalApprovalId) {
      removeFromQueue(lastModalApprovalId);
      lastModalApprovalId = null;
    }
  });
</script>

<div class="app">
  <IconRail bind:route onSettings={() => (settingsOpen = true)} />

  <div class="shell">
    <header>
      <span class="logo">Haily</span>
      <span class="subtitle">trợ lý ảo</span>
    </header>

    <Settings bind:open={settingsOpen} sessionIds={getSessionIds} />
    <!-- Out-of-session fallback ONLY (P04, D6): while the chat route is mounted, the
         in-session surface is `ChatStream`'s inline `ApprovalQueue` instead — this modal
         must not double up with it. -->
    {#if route !== 'chat'}
      <ApprovalModal bind:pending={pendingApproval} />
    {/if}

    <!-- The workspace pane (View Engine Phase A) coexists with whichever destination is
         active — it is a side region, never a replacement for the main content (Creative
         Director HIGH: a presented view must not become a chat bubble or hide the
         conversation). -->
    <div class="body">
      <div class="main-content">
        {#if route === 'chat'}
          <WorkItemsPanel />
          <ProactivePanel />
          <ChatStream
            {messages}
            {jobsState}
            approvals={approvalQueue}
            onApprovalResolved={removeFromQueue}
            bind:bottomAnchor
          />
          <ChatInput
            {messages}
            {sessionIndex}
            {seenSessionIds}
            {pendingApproval}
            bind:activeSession
            bind:stopping
            {scrollToBottom}
          />
        {:else if route === 'runs'}
          <RunsScreen />
        {:else if route === 'workspaces'}
          <WorkspacesScreen />
        {:else if route === 'skills'}
          <SkillsScreen />
        {/if}
      </div>

      <WorkspacePane bind:pendingView={pendingWorkspaceView} />
    </div>
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
    flex-direction: row;
    height: 100dvh;
  }

  .shell {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
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

  /* Horizontal row: the active destination's content + the workspace pane side region.
     Only takes the space below the header; each child manages its own vertical
     layout/scrolling as before this wrapper was introduced. */
  .body {
    display: flex;
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }

  .main-content {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }

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
</style>
