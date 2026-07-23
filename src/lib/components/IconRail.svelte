<script module lang="ts">
  export type RouteId = 'chat' | 'runs' | 'workspaces' | 'skills';
</script>

<script lang="ts">
  interface Destination {
    id: RouteId;
    label: string;
    icon: string;
  }

  const DESTINATIONS: Destination[] = [
    { id: 'chat', label: 'Trò chuyện', icon: '💬' },
    { id: 'runs', label: 'Tác vụ', icon: '▶' },
    { id: 'workspaces', label: 'Không gian làm việc', icon: '🗂' },
    { id: 'skills', label: 'Kỹ năng', icon: '✨' },
  ];

  interface Props {
    route: RouteId;
    onSettings: () => void;
    /** Per-destination badge counts — empty until P07 wires live-run/pending-approval
     * counts onto the Runs destination; a route absent from this map renders no badge. */
    badges?: Partial<Record<RouteId, number>>;
  }

  let { route = $bindable('chat'), onSettings, badges = {} }: Props = $props();
</script>

<nav class="rail" aria-label="Điều hướng chính">
  {#each DESTINATIONS as dest (dest.id)}
    <button
      class="dest"
      class:active={route === dest.id}
      onclick={() => (route = dest.id)}
      title={dest.label}
      aria-label={dest.label}
      aria-current={route === dest.id ? 'page' : undefined}
    >
      <span class="icon">{dest.icon}</span>
      {#if badges[dest.id]}
        <span class="badge">{badges[dest.id]}</span>
      {/if}
    </button>
  {/each}

  <button class="dest settings" onclick={onSettings} title="Cài đặt" aria-label="Cài đặt">
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <circle cx="12" cy="12" r="3"/>
      <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>
    </svg>
  </button>
</nav>

<style>
  .rail {
    width: 60px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    padding: 14px 0;
    background: #131320;
    border-right: 1px solid #1e1e2e;
  }

  .dest {
    width: 40px;
    height: 40px;
    border: none;
    border-radius: 10px;
    background: transparent;
    color: #6b6b8a;
    font-size: 17px;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    position: relative;
    transition: color 0.15s, background 0.15s;
  }

  .dest:hover:not(.active) { color: #a09ac0; background: #1a1a2e; }
  .dest.active { color: #c084fc; background: #2a2a45; }

  .settings {
    margin-top: auto;
    color: #4a4a6a;
    font-size: 18px;
  }
  .settings:hover { color: #c084fc; background: #1a1a2e; }

  .badge {
    position: absolute;
    top: 2px;
    right: 2px;
    min-width: 15px;
    height: 15px;
    padding: 0 3px;
    border-radius: 999px;
    background: #7c3aed;
    color: #fff;
    font-size: 10px;
    font-weight: 700;
    display: flex;
    align-items: center;
    justify-content: center;
    line-height: 1;
  }
</style>
