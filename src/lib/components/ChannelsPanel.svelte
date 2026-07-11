<script lang="ts">
  // Active channels + kill switch + ACP pairing entry point (P11b). No `list_channels`
  // Tauri command exists yet (not in the P11a wrapper set) — this does NOT invent one;
  // channel status besides "this GUI window" is a documented follow-up (see report).
  // The kill switch reuses the SAME `safety.disable_writes` preference `SafetyTab.svelte`
  // already toggles — one mechanism, surfaced prominently here too since a remote/
  // background run needs it reachable fastest from the cockpit.
  import { getPreferences, setPreference } from '$lib/tauri';

  let prefs = $state<Record<string, string>>({});
  let loading = $state(true);
  let toggling = $state(false);
  let error = $state('');
  // P12: ACP editor pairing. Haily runs as the agent BEHIND the editor — the editor spawns
  // `haily acp` over stdio, so pairing is a one-time editor config, not a GUI-launched action.
  let showAcp = $state(false);

  $effect(() => {
    load();
  });

  async function load() {
    loading = true;
    error = '';
    try {
      prefs = await getPreferences();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  const writesPaused = () => prefs['safety.disable_writes'] === 'true' || prefs['safety.disable_writes'] === '1';

  async function toggleWrites() {
    if (toggling) return;
    toggling = true;
    error = '';
    try {
      await setPreference('safety.disable_writes', writesPaused() ? 'false' : 'true');
      await load();
    } catch (e) {
      error = String(e);
    } finally {
      toggling = false;
    }
  }
</script>

<div class="section">
  <div class="block kill-switch">
    <div class="switch-copy">
      <span class="switch-title">⛔ Kill switch — pause all writes</span>
      <span class="hint">
        Stops Haily from making any new change, across every channel (GUI, Telegram,
        background), until you turn it back on.
      </span>
    </div>
    {#if loading}
      <span class="hint">Loading…</span>
    {:else}
      <button
        class="switch"
        class:on={writesPaused()}
        role="switch"
        aria-checked={writesPaused()}
        aria-label="Pause all writes"
        disabled={toggling}
        onclick={toggleWrites}
      >
        <span class="knob"></span>
      </button>
    {/if}
  </div>
  {#if error}<div class="status-error">⚠️ {error}</div>{/if}

  <div class="block">
    <span class="switch-title">Channels</span>
    <div class="channel-row">
      <span class="badge on">GUI</span>
      <span class="hint">This window — active while open.</span>
    </div>
    <div class="channel-row">
      <span class="badge unknown">Telegram</span>
      <span class="hint">Live status isn't wired to the cockpit yet — check Settings for whether it's configured.</span>
    </div>
  </div>

  <div class="block">
    <span class="switch-title">Editors</span>
    <span class="hint">
      Follow Haily's code changes as native inline diffs in an ACP-capable editor (e.g. Zed).
      Haily runs as the agent behind the editor.
    </span>
    <button class="acp-btn" onclick={() => (showAcp = !showAcp)}>
      {showAcp ? 'Hide pairing steps' : 'Pair ACP editor'}
    </button>
    {#if showAcp}
      <div class="acp-steps">
        <p>Add Haily as a custom ACP agent in your editor, pointing it at:</p>
        <code>haily acp</code>
        <p class="hint">
          The editor launches this over stdio; approvals and the plan checkpoint appear as
          native editor permission prompts, and the kill switch above still applies.
        </p>
      </div>
    {/if}
  </div>
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 16px; }
  .block { display: flex; flex-direction: column; gap: 8px; }

  .kill-switch { display: flex; flex-direction: row; align-items: center; justify-content: space-between; gap: 12px; }
  .switch-copy { display: flex; flex-direction: column; gap: 4px; }
  .switch-title { font-size: 13px; color: #e0dff5; font-weight: 600; }
  .hint { font-size: 11px; color: #6b6b8a; line-height: 1.5; }

  .channel-row { display: flex; align-items: center; gap: 8px; }

  .badge {
    flex-shrink: 0;
    font-size: 10px;
    padding: 2px 8px;
    border-radius: 999px;
    background: #1e1e35;
    border: 1px solid #2e2e4a;
    color: #a09ac0;
  }
  .badge.on { color: #4ade80; border-color: #166534; }
  .badge.unknown { color: #6b6b8a; }

  .acp-btn {
    align-self: flex-start;
    padding: 6px 14px;
    min-height: 32px;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
    background: #16162a;
    color: #a09ac0;
    font-size: 12px;
    cursor: pointer;
  }
  .acp-btn:hover { border-color: #4b4b6a; color: #e0dff5; }

  .acp-steps { display: flex; flex-direction: column; gap: 6px; font-size: 12px; color: #a09ac0; }
  .acp-steps p { margin: 0; }
  .acp-steps code {
    align-self: flex-start;
    padding: 4px 8px;
    border-radius: 6px;
    background: #0f0f1e;
    border: 1px solid #2e2e4a;
    color: #4ade80;
    font-size: 12px;
  }

  .switch {
    flex-shrink: 0;
    width: 42px;
    height: 24px;
    border-radius: 999px;
    border: 1px solid #7f1d1d;
    background: #16162a;
    cursor: pointer;
    position: relative;
  }
  .switch:disabled { opacity: 0.5; cursor: default; }
  .switch .knob {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: #6b6b8a;
    transition: transform 0.15s, background 0.15s;
  }
  .switch.on { background: #7f1d1d; border-color: #f87171; }
  .switch.on .knob { transform: translateX(18px); background: #fff; }

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
