<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { onProactiveCards, sendMessage, type ProactiveCard } from '$lib/tauri';

  // Backend snapshot as last delivered — see `onProactiveCards`'s doc comment for why
  // there is no on-mount reconcile fetch here (best-effort delivery, no `list_*`).
  let cards = $state<ProactiveCard[]>([]);
  // Dismissal is PURELY local/ephemeral (per the phase-08 architecture note): the
  // backend has no concept of "dismissed" and keeps forwarding its own accumulated,
  // per-kind-capped snapshot. A dismissed card can therefore reappear if the backend
  // resends the whole snapshot for an unrelated reason before that card is evicted —
  // acceptable for a best-effort proactive surface, not a correctness bug.
  let dismissedIds = $state<Set<string>>(new Set());
  let unlisten: (() => void) | undefined;

  const visible = $derived(cards.filter((c) => !dismissedIds.has(c.id)));

  onMount(() => {
    const unlistenPromise = onProactiveCards((snapshot) => { cards = snapshot; });
    unlisten = () => { unlistenPromise.then((fn) => fn()); };
  });

  onDestroy(() => unlisten?.());

  function dismiss(id: string) {
    dismissedIds = new Set(dismissedIds).add(id);
  }

  function formatTime(rfc3339: string): string {
    const d = new Date(rfc3339);
    return Number.isNaN(d.getTime())
      ? ''
      : d.toLocaleTimeString('vi-VN', { hour: '2-digit', minute: '2-digit' });
  }

  let viewing = $state<string | null>(null);

  /** "Link to its reminder" (success criteria): the app has no dedicated reminder
   * detail view, so this reuses the existing chat pipe — the same pattern
   * `+page.svelte`'s inline undo button uses — rather than inventing a new route
   * just for this one card kind (YAGNI). */
  async function viewReminder(reminderId: string, title: string) {
    if (viewing) return;
    viewing = reminderId;
    try {
      await sendMessage(`Cho tôi biết chi tiết về nhắc nhở "${title}" (id: ${reminderId}).`);
    } catch (e) {
      console.error('viewReminder sendMessage failed', e);
    } finally {
      viewing = null;
    }
  }
</script>

{#if visible.length > 0}
  <div class="proactive-panel" role="log" aria-label="Thông báo chủ động">
    {#each visible as card (card.id)}
      <div class="card" class:urgent={card.kind.type === 'Alert' && card.kind.data.urgent}>
        <button class="dismiss" onclick={() => dismiss(card.id)} aria-label="Đóng thông báo" title="Đóng">×</button>
        {#if card.kind.type === 'MorningBrief'}
          <div class="head"><span class="icon">🌅</span><span class="label">Bản tin buổi sáng</span></div>
          <p class="body">{card.kind.data.text}</p>
        {:else if card.kind.type === 'Alert'}
          <div class="head">
            <span class="icon">{card.kind.data.urgent ? '🔴' : '📢'}</span>
            <span class="label">{card.kind.data.title}</span>
          </div>
          <p class="body">{card.kind.data.body}</p>
        {:else if card.kind.type === 'ReminderFired'}
          {@const reminder = card.kind.data}
          <div class="head"><span class="icon">⏰</span><span class="label">Nhắc nhở</span></div>
          <p class="body">{reminder.title}</p>
          <button
            class="link-action"
            disabled={viewing === reminder.reminder_id}
            onclick={() => viewReminder(reminder.reminder_id, reminder.title)}
          >
            {viewing === reminder.reminder_id ? 'Đang mở…' : 'Xem chi tiết'}
          </button>
        {/if}
        <span class="time">{formatTime(card.created_at)}</span>
      </div>
    {/each}
  </div>
{/if}

<style>
  .proactive-panel {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px 16px;
    border-bottom: 1px solid #1e1e2e;
    flex-shrink: 0;
  }

  .card {
    position: relative;
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 10px;
    padding: 8px 28px 8px 10px;
  }

  .card.urgent {
    border-color: #7c2d3a;
    background: #23151a;
  }

  .head {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    font-weight: 600;
    color: #c084fc;
  }

  .card.urgent .head { color: #f87171; }

  /* Card text comes from user/task content — rendered as plain text nodes (never
     bound via {@html}) so nothing in a brief/alert/reminder body can inject markup. */
  .body {
    margin: 4px 0 0;
    font-size: 12px;
    color: #ddd8f5;
    white-space: pre-wrap;
    word-break: break-word;
  }

  .time {
    display: block;
    margin-top: 4px;
    font-size: 10px;
    color: #6b6b8a;
  }

  .dismiss {
    position: absolute;
    top: 4px;
    right: 6px;
    width: 20px;
    height: 20px;
    border: none;
    background: transparent;
    color: #6b6b8a;
    font-size: 14px;
    line-height: 1;
    cursor: pointer;
    border-radius: 4px;
  }

  .dismiss:hover { color: #ddd8f5; background: #2a2a45; }

  .link-action {
    margin-top: 6px;
    padding: 4px 10px;
    min-height: 28px;
    border: 1px solid #3a2a5a;
    border-radius: 999px;
    background: #1e1638;
    color: #c084fc;
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
  }

  .link-action:hover:not(:disabled) { border-color: #7c3aed; background: #271a4a; }
  .link-action:disabled { opacity: 0.5; cursor: default; }
</style>
