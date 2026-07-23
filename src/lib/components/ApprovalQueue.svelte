<script lang="ts">
  // Shared inline approval queue rendered in the chat flow (D6) — replaces the modal as
  // the PRIMARY approval surface while the chat route is mounted; `ApprovalModal` stays
  // only for out-of-session approvals (`+page.svelte` gates it to non-chat routes).
  // `approvals` accumulates every `ToolApprovalRequest` chunk this GUI window observed
  // (any session/run, incl. a background pipeline run's checkpoint prompt — both ride the
  // same chunk, see `tool_call.rs`/`runner.rs`), so a launch from ANY route funnels here
  // once the user comes back to chat.
  //
  // `resolveApproval` is idempotent (returns `false` for an already-resolved id, never
  // throws) — this component treats BOTH a successful resolve and a stale no-op the same
  // way: remove the card and tell the parent via `onResolved`. This is what makes it safe
  // for the same approval to also have been resolved via the out-of-session modal in the
  // meantime (risk assessment, phase-04 doc: "a double call is safe").
  import { resolveApproval, type PendingApproval } from '$lib/tauri';
  import { toolVerb } from '$lib/tool-verbs';

  let { approvals, onResolved }: { approvals: PendingApproval[]; onResolved: (approvalId: string) => void } =
    $props();

  // Mirrors `ApprovalModal.svelte:27-28`'s own two badge strings VERBATIM (kept in sync
  // by hand — not exported from that file, which this phase does not own/modify) so the
  // same approval reads identically whichever surface renders it. Review fix LOW-7: this
  // previously drifted ("cần bạn xác nhận" was dropped from the reversible-case wording).
  const FINAL_BADGE = 'Không thể hoàn tác';
  const CAPPED_BADGE = 'Đã đạt giới hạn — cần bạn xác nhận (vẫn hoàn tác được)';

  // Review fix LOW-5: `resolving` (not a single `resolvingId`) disables EVERY row's
  // buttons while any one approval is in flight — the previous `resolvingId === a.id`
  // check only visually disabled the row actually resolving, silently swallowing a click
  // on a different row via the `if (resolvingId) return;` guard with no feedback.
  let resolving = $state(false);

  async function decide(a: PendingApproval, approved: boolean) {
    if (resolving) return;
    resolving = true;
    try {
      await resolveApproval(a.sessionId, a.approvalId, approved);
    } catch (e) {
      console.error('resolveApproval failed', e);
    } finally {
      resolving = false;
      onResolved(a.approvalId);
    }
  }
</script>

{#if approvals.length > 0}
  <div class="queue">
    <div class="queue-header">
      <span class="badge">{approvals.length}</span>
      <span class="label">yêu cầu phê duyệt đang chờ</span>
    </div>
    {#each approvals as a (a.approvalId)}
      <div class="row">
        <span class="tier-badge" class:reversible={a.reversible}>
          ⚠ {a.reversible ? CAPPED_BADGE : FINAL_BADGE}
        </span>
        <p class="verb">{toolVerb(a.tool, a.args)}</p>
        <p class="tool-name"><code>{a.tool}</code></p>
        {#if a.origin}
          <p class="origin">Yêu cầu bởi: <code>{a.origin}</code></p>
        {/if}
        <div class="actions">
          <button class="deny" onclick={() => decide(a, false)} disabled={resolving}>❌ Không</button>
          <button class="approve" onclick={() => decide(a, true)} disabled={resolving}>✅ Có</button>
        </div>
      </div>
    {/each}
  </div>
{/if}

<style>
  .queue {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px 12px;
    border-bottom: 1px solid #1e1e2e;
    flex-shrink: 0;
  }

  .queue-header { display: flex; align-items: center; gap: 6px; }
  .badge {
    min-width: 18px;
    height: 18px;
    padding: 0 5px;
    border-radius: 999px;
    background: #7c3aed;
    color: #fff;
    font-size: 11px;
    font-weight: 700;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .label { font-size: 11px; color: #a09ac0; }

  .row {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 8px 10px;
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 10px;
  }

  .tier-badge {
    align-self: flex-start;
    background: #3a1f2e;
    color: #f87171;
    border: 1px solid #7f1d1d;
    border-radius: 999px;
    font-size: 10px;
    font-weight: 700;
    padding: 2px 8px;
  }
  .tier-badge.reversible { background: #2e2a1f; color: #eab308; border-color: #7f6a1d; }

  .verb { font-size: 12px; color: #e0dff5; font-weight: 600; }
  .tool-name { font-size: 11px; color: #c084fc; }
  .origin { font-size: 11px; color: #8a86ac; }
  .origin code, .tool-name code { color: inherit; }

  .actions { display: flex; gap: 8px; margin-top: 2px; }
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
</style>
