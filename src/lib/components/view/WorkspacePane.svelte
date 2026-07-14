<script lang="ts">
  // Resizable, right-anchored pane rendering a `DataView` (View Engine Phase A). Fetch-then-
  // render (mirrors `WorkItemsPanel`'s fetch-on-mount, NOT `RunTimeline`'s pure event stream):
  // the chat `ViewRef` chunk is only a handle, the full payload always comes from `getView`.
  //
  // Resize uses the native CSS `resize: horizontal` affordance (see `.workspace-pane` below)
  // rather than a hand-rolled pointer-drag handler bound through an inline `style={width}` —
  // that would trip the SEC F1 grep-gate's `style={` ban in this directory for zero benefit,
  // since the browser's own resize handle does the same job with no JS.
  import { getView, recordViewIntent, type DataView, type ProjectionSpec } from '$lib/tauri';
  import ViewRenderer from './ViewRenderer.svelte';
  import ProjectionSwitcher from './ProjectionSwitcher.svelte';
  import ViewMeasurementBar from './ViewMeasurementBar.svelte';

  /** Set by the parent to a fresh `{ viewId, sessionId }` each time a `ViewRefChunk` arrives on
   * the chat stream; consumed (cleared back to `null`) on the same tick — mirrors
   * `ApprovalModal`'s `bind:pending` contract. `sessionId` rides in from the caller because
   * neither `DataView` nor `ViewRefChunk` carry a session id of their own (Phase 1's frozen
   * wire contract) — it's the `ChunkPayload` envelope's `session_id` instead. */
  let { pendingView = $bindable<{ viewId: string; sessionId: string } | null>(null) } = $props();

  // View-id back-stack (Phase A: push-only navigation; Reference drill-in is out of scope —
  // see phase file). `viewSessions` is a plain side-map from view_id to the session that
  // presented it (needed for `recordViewIntent`'s auth-boundary param) — kept separate so the
  // stack itself stays the simple id list the design calls for.
  let stack = $state<string[]>([]);
  const viewSessions = new Map<string, string>();

  let currentView = $state<DataView | null>(null);
  let expired = $state(false);
  let loading = $state(false);
  let openedAt = $state<number | null>(null);

  const currentViewId: string | null = $derived(stack[stack.length - 1] ?? null);

  $effect(() => {
    if (pendingView) {
      const { viewId, sessionId } = pendingView;
      pendingView = null; // consume immediately — a repeated ViewRef for the same id still re-pushes
      viewSessions.set(viewId, sessionId);
      stack = [...stack, viewId];
      openView(viewId);
    }
  });

  async function openView(viewId: string) {
    loading = true;
    expired = false;
    currentView = null;
    try {
      const view = await getView(viewId);
      if (!view) {
        expired = true;
        return;
      }
      currentView = view;
      openedAt = Date.now();
      const sessionId = viewSessions.get(viewId);
      if (sessionId) {
        recordViewIntent(viewId, sessionId, 'viewed').catch((e) =>
          console.error('recordViewIntent(viewed) failed', e),
        );
      }
    } catch (e) {
      console.error('getView failed', e);
      expired = true;
    } finally {
      loading = false;
    }
  }

  function goBack() {
    if (stack.length <= 1) {
      stack = [];
      currentView = null;
      expired = false;
      return;
    }
    stack = stack.slice(0, -1);
    const prevId = stack[stack.length - 1];
    if (prevId) openView(prevId);
  }

  function switchProjection(spec: ProjectionSpec) {
    if (!currentView || !currentViewId) return;
    // Client-side only — the already-fetched `DataView` carries every projection's data, so no
    // refetch is needed. `$state` deep-proxies plain objects, so mutating `.active` in place is
    // enough to re-render `ViewRenderer` (which derives off `view.active.kind`).
    currentView.active = spec;
    const sessionId = viewSessions.get(currentViewId);
    if (sessionId) {
      recordViewIntent(currentViewId, sessionId, 'projection_switched', spec.kind).catch((e) =>
        console.error('recordViewIntent(projection_switched) failed', e),
      );
    }
  }

  function handleEditDemand(answer: string) {
    if (!currentViewId) return;
    const sessionId = viewSessions.get(currentViewId);
    if (!sessionId) return;
    recordViewIntent(currentViewId, sessionId, 'edit_demand', answer).catch((e) =>
      console.error('recordViewIntent(edit_demand) failed', e),
    );
  }

  function handleThumb(direction: '+' | '-') {
    if (!currentViewId) return;
    const sessionId = viewSessions.get(currentViewId);
    if (!sessionId) return;
    recordViewIntent(currentViewId, sessionId, 'usefulness', direction).catch((e) =>
      console.error('recordViewIntent(usefulness) failed', e),
    );
  }
</script>

{#if stack.length > 0}
  <div class="workspace-pane">
    <div class="header">
      <div class="title-row">
        <button class="back-btn" onclick={goBack} disabled={loading} title="Quay lại" aria-label="Quay lại">‹</button>
        <span class="entity">{currentView?.entity ?? '…'}</span>
        {#if currentView}
          <span class="badge" role="note">⚠ AI-generated view — verify against source</span>
        {/if}
      </div>
      {#if currentView}
        <ProjectionSwitcher
          projections={currentView.projections}
          active={currentView.active}
          onSwitch={switchProjection}
        />
        <ViewMeasurementBar onEditDemand={handleEditDemand} onThumb={handleThumb} {openedAt} />
      {/if}
    </div>

    <div class="content">
      {#if loading}
        <div class="empty">Đang tải…</div>
      {:else if expired}
        <div class="empty">Chế độ xem đã hết hạn.</div>
      {:else if currentView}
        <ViewRenderer view={currentView} />
      {/if}
    </div>
  </div>
{/if}

<style>
  /* `resize: horizontal` gives the pane a native drag handle at its trailing edge — no
     hand-rolled pointer-drag JS, no `style={width}` binding (see script-block comment). */
  .workspace-pane {
    display: flex;
    flex-direction: column;
    width: 420px;
    min-width: 280px;
    max-width: 720px;
    flex-shrink: 0;
    resize: horizontal;
    overflow: auto;
    border-left: 1px solid #1e1e2e;
    background: #0f0f12;
  }

  .header {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px 14px;
    border-bottom: 1px solid #1e1e2e;
    flex-shrink: 0;
  }

  .title-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }

  .back-btn {
    width: 26px;
    height: 26px;
    border-radius: 6px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #a09ac0;
    font-size: 16px;
    cursor: pointer;
    flex-shrink: 0;
  }
  .back-btn:hover:not(:disabled) { color: #c084fc; border-color: #4a3a7a; }
  .back-btn:disabled { opacity: 0.5; cursor: default; }

  .entity {
    font-size: 14px;
    font-weight: 600;
    color: #e0dff5;
    text-transform: capitalize;
  }

  /* Required trust signal (design §14 / Social Scientist badge-blindness mitigation) — visually
     distinct, never a dismissible toast, no close button. */
  .badge {
    font-size: 10px;
    font-weight: 700;
    letter-spacing: 0.01em;
    color: #eab308;
    background: #2e2a1f;
    border: 1px solid #7f6a1d;
    border-radius: 999px;
    padding: 3px 9px;
  }

  .content {
    flex: 1;
    min-height: 0;
    overflow: auto;
    padding: 12px 14px;
  }

  .empty {
    font-size: 12px;
    color: #6b6b8a;
    padding: 16px;
    text-align: center;
  }
</style>
