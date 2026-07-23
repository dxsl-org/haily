<script lang="ts">
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
  import type { PendingApproval } from '$lib/tauri';

  let settingsOpen = $state(false);
  // Left icon rail (Chat/Runs/Workspaces/Skills) replaces the former chat/cockpit
  // toggle — Settings stays a drawer opened by the rail's gear, not a route, so its
  // overlay behavior is unchanged.
  let route = $state<RouteId>('chat');
  let pendingApproval = $state<PendingApproval | null>(null);
  // Set for one tick whenever a `ViewRef` chunk arrives (written by `ChatStream`);
  // `WorkspacePane` consumes and clears it via `bind:pendingView` (mirrors
  // `pendingApproval`'s `bind:pending` contract on `ApprovalModal`). The pane coexists
  // with whichever destination is active — it is never the thing that hides a route.
  let pendingWorkspaceView = $state<{ viewId: string; sessionId: string } | null>(null);
  // The session_id of the turn currently streaming, or null when idle. Written by
  // `ChatInput` on send, cleared by `ChatStream` when that session's `Complete`/`Error`
  // chunk arrives — shared between the two so each can gate/react independently.
  let activeSession = $state<string | null>(null);
  // Transient "stop requested" flag — set by `ChatInput`'s stop button, cleared by
  // `ChatStream` once the cancelled turn's `Complete` chunk actually lands.
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
  // `ChatInput.send()`, read/cleared by `ChatStream`'s chunk listener.
  const sessionIndex = new Map<string, number>();

  // Every session id this GUI instance has started, oldest first — each turn mints a
  // fresh UUID (see `ChatInput.send()`), so there is no single "current session" the
  // Safety tab's recent-actions list could scope to. Passed to `Settings` as a getter so
  // it always reads the live array rather than a snapshot taken when the drawer opened.
  const seenSessionIds: string[] = [];
  const getSessionIds = () => seenSessionIds;

  function scrollToBottom() {
    requestAnimationFrame(() => bottomAnchor?.scrollIntoView({ behavior: 'smooth' }));
  }
</script>

<div class="app">
  <IconRail bind:route onSettings={() => (settingsOpen = true)} />

  <div class="shell">
    <header>
      <span class="logo">Haily</span>
      <span class="subtitle">trợ lý ảo</span>
    </header>

    <Settings bind:open={settingsOpen} sessionIds={getSessionIds} />
    <ApprovalModal bind:pending={pendingApproval} />

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
            {sessionIndex}
            bind:pendingApproval
            bind:pendingWorkspaceView
            bind:activeSession
            bind:stopping
            bind:bottomAnchor
            {scrollToBottom}
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
