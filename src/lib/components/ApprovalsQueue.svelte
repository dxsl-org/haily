<script lang="ts">
  // Unified inbox of pending approvals from ANY channel + resolve action (P11b). The
  // transient per-turn `ApprovalModal` in `+page.svelte` stays for the ACTIVE prompt on
  // THIS GUI session — this queue is the cross-channel audit/inbox view (Telegram/TUI/
  // ACP approvals show up here too, since the broker is shared).
  import { onMount, onDestroy } from 'svelte';
  import { listApprovals, resolveApproval, type QueuedApproval } from '$lib/tauri';

  // Lets whichever parent mounts this alongside `WorkspacePanel` mirror the freshest
  // snapshot into it (diff-accept correlation) without a second fetch.
  let { onUpdate }: { onUpdate?: (approvals: QueuedApproval[]) => void } = $props();

  let approvals = $state<QueuedApproval[]>([]);
  let loading = $state(true);
  let error = $state('');
  let resolvingId = $state<string | null>(null);
  let pollHandle: ReturnType<typeof setInterval> | undefined;

  async function load() {
    error = '';
    try {
      approvals = await listApprovals();
      onUpdate?.(approvals);
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    load();
    // No push channel exists for this queue (unlike work-items/proactive) — the broker
    // is a plain reconcile snapshot with no descriptive payload (P11a deviation log:
    // `PendingApproval { approval_id, session_id, created_at }`, no tool/args). Poll at a
    // modest interval since this is an audit/inbox view, not the primary interaction —
    // the single ACTIVE approval still gets the instant chunk + blocking modal.
    pollHandle = setInterval(load, 5000);
  });

  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
  });

  async function decide(a: QueuedApproval, approved: boolean) {
    if (resolvingId) return;
    resolvingId = a.approval_id;
    error = '';
    try {
      await resolveApproval(a.session_id, a.approval_id, approved);
      await load();
    } catch (e) {
      error = String(e);
    } finally {
      resolvingId = null;
    }
  }

  function formatTime(rfc3339: string): string {
    const d = new Date(rfc3339);
    return Number.isNaN(d.getTime()) ? rfc3339 : d.toLocaleTimeString('vi-VN', { hour: '2-digit', minute: '2-digit' });
  }
</script>

<div class="section">
  <div class="list-header">
    <span class="switch-title">Approvals queue</span>
    <button class="refresh-btn" onclick={load} disabled={loading} title="Refresh">↻</button>
  </div>
  {#if loading}
    <div class="empty">Loading…</div>
  {:else if error}
    <div class="status-error">⚠️ {error}</div>
  {:else if approvals.length === 0}
    <div class="empty">No pending approvals.</div>
  {:else}
    <div class="rows">
      {#each approvals as a (a.approval_id)}
        <div class="row">
          <div class="head">
            <span class="approval-id">…{a.approval_id.slice(-8)}</span>
            <span class="time">{formatTime(a.created_at)}</span>
          </div>
          <span class="hint">Session …{a.session_id.slice(-8)} — full details are in that channel's own prompt.</span>
          <div class="actions">
            <button class="deny" onclick={() => decide(a, false)} disabled={resolvingId === a.approval_id}>Deny</button>
            <button class="approve" onclick={() => decide(a, true)} disabled={resolvingId === a.approval_id}>Approve</button>
          </div>
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 10px; }

  .list-header { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
  .switch-title { font-size: 13px; color: #e0dff5; font-weight: 600; }

  .refresh-btn {
    flex-shrink: 0;
    width: 30px;
    padding: 4px 0;
    text-align: center;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
    background: #16162a;
    color: #8884aa;
    font-size: 13px;
    cursor: pointer;
  }
  .refresh-btn:hover:not(:disabled) { color: #c084fc; border-color: #4a3a7a; }
  .refresh-btn:disabled { opacity: 0.5; cursor: default; }

  .rows { display: flex; flex-direction: column; gap: 8px; }

  .row {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 10px;
    background: #0f0f18;
    border: 1px solid #23233a;
    border-radius: 8px;
  }
  .head { display: flex; justify-content: space-between; align-items: baseline; }
  .approval-id { font-size: 11px; color: #c084fc; font-family: ui-monospace, monospace; }
  .time { font-size: 10px; color: #4a4a6a; }
  .hint { font-size: 11px; color: #6b6b8a; line-height: 1.5; }

  .actions { display: flex; gap: 8px; }
  .actions button {
    padding: 5px 12px;
    min-height: 32px;
    border-radius: 7px;
    border: none;
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
  }
  .actions button:disabled { opacity: 0.5; cursor: default; }
  .deny { background: #2a2a45; color: #ddd8f5; }
  .approve { background: #7c3aed; color: #fff; }

  .empty {
    font-size: 12px;
    color: #6b6b8a;
    padding: 10px;
    background: #0f0f18;
    border-radius: 8px;
    border: 1px dashed #2e2e4a;
  }

  .status-error {
    font-size: 11px;
    padding: 6px 10px;
    border-radius: 6px;
    background: #2a0f0f;
    color: #f87171;
    border: 1px solid #7f1d1d;
    word-break: break-word;
  }
</style>
