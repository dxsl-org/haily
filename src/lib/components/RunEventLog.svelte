<script lang="ts">
  // Capped, narrated event log for a run's expanded detail — split out of
  // `RunProgressCard` (P04) so the card itself stays under the file-size budget; also
  // reusable as-is by P07's Runs-screen row detail (same reducer state, same narration).
  import { describeEvent } from '$lib/run-events';
  import { narrate } from '$lib/run-narration';
  import type { RunEvent } from '$lib/tauri';

  let { events, max = 8 }: { events: RunEvent[]; max?: number } = $props();

  const visible = $derived(events.slice(Math.max(0, events.length - max)));
</script>

<div class="events">
  {#each visible as event, i (i)}
    {@const d = describeEvent(event)}
    <div class="event tone-{d.tone}">
      <span class="icon">{d.icon}</span>
      <!-- `narrate()` returns fixed VN vocabulary derived from the event KIND only, never
           raw tool/model payload — safe as plain text, never {@html}. -->
      <span class="text">{narrate(event)}</span>
    </div>
  {/each}
</div>

<style>
  .events {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding-top: 6px;
    border-top: 1px solid #1e1e2e;
    max-height: 220px;
    overflow-y: auto;
  }

  .event {
    display: flex;
    gap: 6px;
    line-height: 1.5;
    color: #a09ac0;
    white-space: pre-wrap;
    word-break: break-word;
    font-size: 12px;
  }
  .event .icon { flex-shrink: 0; }
  .event.tone-pass { color: #4ade80; }
  .event.tone-fail { color: #f87171; }
  .event.tone-warn { color: #fbbf24; }
</style>
