<script lang="ts">
  // Distinguishes "desktop unreachable" vs "auth rejected" vs "identity changed — re-pair"
  // (m5) — same underlying cert-mismatch check as an active-MITM warning, but the copy/tone
  // must not cry wolf on a routine cert rotation (docs/mobile-protocol.md §4).
  import type { MobileConnectionState } from './mobile-tauri';

  let { state }: { state: MobileConnectionState } = $props();

  const COPY: Record<NonNullable<MobileConnectionState['reason']>, { icon: string; text: string }> = {
    unreachable: {
      icon: '📡',
      text: "Can't reach your computer — check Tailscale is running there and on this phone.",
    },
    auth_rejected: {
      icon: '🔒',
      text: 'This device was revoked or its login expired. Re-pair from your computer.',
    },
    re_pair: {
      icon: '🔁',
      text: "Your computer's identity changed (likely a routine security refresh) — re-pair to continue.",
    },
  };
</script>

{#if state.connected}
  <div class="banner ok" role="status">🟢 Connected</div>
{:else if state.reason}
  <div class="banner warn" role="alert">
    <span class="icon">{COPY[state.reason].icon}</span>
    <span>{COPY[state.reason].text}</span>
  </div>
{:else}
  <div class="banner pending" role="status">⏳ Connecting…</div>
{/if}

<style>
  .banner {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 14px;
    font-size: 12px;
    flex-shrink: 0;
  }
  .banner.ok { background: #12241a; color: #4ade80; }
  .banner.pending { background: #16162a; color: #a09ac0; }
  .banner.warn { background: #2a1f0f; color: #fbbf24; line-height: 1.4; }
  .icon { flex-shrink: 0; }
</style>
