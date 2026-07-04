<script lang="ts">
  // Blocking modal for a pending `ToolApprovalRequest` chunk (Phase 4 — real
  // approval broker). The backend's `ApprovalBroker::request` is awaiting a
  // decision (or the 120s/cancel deny) while this is open; approve/deny here maps
  // 1:1 to `resolveApproval` — there is no other way to unblock the pending turn
  // from the GUI.
  import { resolveApproval, type PendingApproval } from '$lib/tauri';
  import { toolVerb } from '$lib/tool-verbs';

  let { pending = $bindable<PendingApproval | null>(null) } = $props();

  let resolving = $state(false);

  // Human-verb headline replaces the raw-JSON `<pre>` (R4). `toolVerb` only reads a
  // fixed whitelist of arg keys per known tool name — never arbitrary JSON — so a
  // crafted title in `args` can only ever land inside the returned string, which
  // Svelte's `{expression}` auto-escapes below. NEVER swap this for `{@html}`.
  let verb = $derived(pending ? toolVerb(pending.tool, pending.args) : '');

  // Every approval that reaches this modal was, at dispatch time, `RiskTier::IrreversibleWrite`
  // — but that tier is reached two ways (see `haily-core::tool_call::dispatch`): a tool that is
  // genuinely irreversible (e.g. `memory_forget`, `worktree_apply`), OR a normally-`ReversibleWrite`
  // delete (task/note/reminder) that got escalated for THIS call because the per-turn delete cap
  // was already hit. `pending.reversible` (server-derived from the tool's OWN tier, pre-escalation
  // — never LLM/task text) distinguishes the two, so the badge only claims "can't be undone" when
  // that is actually true.
  const FINAL_BADGE = "Không thể hoàn tác";
  const CAPPED_BADGE = 'Đã đạt giới hạn — cần bạn xác nhận (vẫn hoàn tác được)';

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
    <span class="tier-badge" class:reversible={pending.reversible}>
      ⚠ {pending.reversible ? CAPPED_BADGE : FINAL_BADGE}
    </span>
    <h2>{verb}</h2>
    <p class="tool-name"><code>{pending.tool}</code></p>
    {#if pending.origin}
      <p class="origin">Requested by: <code>{pending.origin}</code></p>
    {/if}
    <div class="actions">
      <button class="deny" onclick={() => decide(false)} disabled={resolving}>❌ Không</button>
      <button class="approve" onclick={() => decide(true)} disabled={resolving}>✅ Có</button>
    </div>
    <p class="promise">Không việc gì tôi làm là không cứu được — hoặc hoàn tác được, hoặc tôi hỏi bạn trước.</p>
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

  /* Cap-escalated but genuinely reversible — softer tone than a true final warning. */
  .tier-badge.reversible {
    background: #2e2a1f;
    color: #eab308;
    border-color: #7f6a1d;
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

  .actions {
    display: flex;
    gap: 10px;
    justify-content: flex-end;
    margin-bottom: 14px;
  }

  .promise {
    font-size: 11px;
    color: #6b6b8a;
    line-height: 1.5;
    border-top: 1px solid #2a2a45;
    padding-top: 10px;
    margin: 0;
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
