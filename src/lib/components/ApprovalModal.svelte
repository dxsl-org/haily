<script lang="ts">
  // Blocking modal for a pending `ToolApprovalRequest` chunk (Phase 4 — real
  // approval broker). The backend's `ApprovalBroker::request` is awaiting a
  // decision (or the 120s/cancel deny) while this is open; approve/deny here maps
  // 1:1 to `resolveApproval` — there is no other way to unblock the pending turn
  // from the GUI.
  import { resolveApproval, type PendingApproval } from '$lib/tauri';

  let { pending = $bindable<PendingApproval | null>(null) } = $props();

  let resolving = $state(false);

  async function decide(approved: boolean) {
    if (!pending || resolving) return;
    resolving = true;
    try {
      await resolveApproval(pending.sessionId, pending.approvalId, approved);
    } finally {
      resolving = false;
      pending = null;
    }
  }
</script>

{#if pending}
  <div class="backdrop" role="presentation"></div>
  <div class="modal" role="alertdialog" aria-modal="true" aria-label="Yêu cầu phê duyệt công cụ">
    <!-- Every approval that reaches this modal is an IrreversibleWrite (Read and
         ReversibleWrite tools never gate — see haily-core::tool_call::dispatch), so
         the badge is a constant, not a per-tool computed value. Plain, non-technical
         copy per the phase-6 spec — no "RiskTier"/"IrreversibleWrite" jargon in the UI. -->
    <span class="tier-badge">⚠ Can't be undone</span>
    <h2>I'll ask you first</h2>
    <p class="tool-name"><code>{pending.tool}</code></p>
    {#if pending.origin}
      <p class="origin">Requested by: <code>{pending.origin}</code></p>
    {/if}
    <pre class="args">{pending.args}</pre>
    <div class="actions">
      <button class="deny" onclick={() => decide(false)} disabled={resolving}>❌ Không</button>
      <button class="approve" onclick={() => decide(true)} disabled={resolving}>✅ Có</button>
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    z-index: 100;
  }

  .modal {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 14px;
    padding: 20px;
    width: min(420px, 90vw);
    z-index: 101;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5);
  }

  .tier-badge {
    display: inline-block;
    background: #3a1f2e;
    color: #f87171;
    border: 1px solid #7f1d1d;
    border-radius: 999px;
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.02em;
    padding: 3px 10px;
    margin-bottom: 10px;
  }

  h2 {
    font-size: 15px;
    color: #e0dff5;
    margin-bottom: 10px;
  }

  .tool-name {
    color: #c084fc;
    font-size: 13px;
    margin-bottom: 8px;
  }

  .origin {
    color: #8a86ac;
    font-size: 12px;
    margin-bottom: 8px;
  }

  .origin code {
    color: #a8a4c8;
  }

  .args {
    background: #0f0f12;
    border: 1px solid #2a2a45;
    border-radius: 8px;
    padding: 10px;
    font-size: 12px;
    color: #a8a4c8;
    max-height: 160px;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-word;
    margin-bottom: 16px;
  }

  .actions {
    display: flex;
    gap: 10px;
    justify-content: flex-end;
  }

  button {
    padding: 8px 16px;
    border-radius: 8px;
    border: none;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    transition: opacity 0.15s;
  }

  button:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .deny {
    background: #2a2a45;
    color: #ddd8f5;
  }

  .approve {
    background: #7c3aed;
    color: #fff;
  }
</style>
