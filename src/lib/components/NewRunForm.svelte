<script lang="ts">
  // GUI "New run" launcher (Pipeline Activation & Wiring phase 3) — the cockpit's only
  // run-INITIATE surface; `RunTimeline` below only ever consumes events a run already
  // produced. Reuses the SAME `haily-run-events` bridge (`onRunEvents`) — launching here
  // needs no new subscription, and no session-filtering: `RunTimeline` renders every
  // observed `run_id` regardless of which session started it.
  import { getPreferences, startCodingRun, type CodingRunKind, type DepthMode } from '$lib/tauri';
  import { open as openDirDialog } from '@tauri-apps/plugin-dialog';

  const DEPTHS: { value: DepthMode; label: string }[] = [
    { value: 'quick', label: 'Quick' },
    { value: 'normal', label: 'Normal' },
    { value: 'deep', label: 'Deep' },
  ];

  let task = $state('');
  let kind = $state<CodingRunKind>('plan');
  let repoPath = $state('');
  let depth = $state<DepthMode>('normal');
  let launching = $state(false);
  let error = $state('');

  // Empty task disables Launch outright (requirement); a missing repo is checked at
  // click-time instead (see `launch()`) so it can surface as the specific inline error
  // the phase spec calls for, rather than a silently-disabled button with no explanation.
  const canLaunch = $derived(task.trim().length > 0 && !launching);

  $effect(() => {
    loadDefaultRepo();
  });

  async function loadDefaultRepo() {
    try {
      const prefs = await getPreferences();
      const def = prefs['coding.default_repo'];
      // Never clobber a repo path the user already typed/picked before this resolved.
      if (def && !repoPath) repoPath = def;
    } catch {
      // No preference configured yet, or the read failed — the picker just starts
      // empty; `launch()`'s own validation still catches a missing repo.
    }
  }

  async function browseRepo() {
    const dir = await openDirDialog({ directory: true, multiple: false });
    if (typeof dir === 'string') repoPath = dir;
  }

  async function launch() {
    if (!canLaunch) return;
    error = '';
    const repo = repoPath.trim();
    if (!repo) {
      error = 'No repo selected — pick one or set a default in Settings.';
      return;
    }
    launching = true;
    try {
      await startCodingRun(kind, task.trim(), repo, depth);
      task = '';
    } catch (e) {
      error = String(e);
    } finally {
      launching = false;
    }
  }
</script>

<div class="section">
  <span class="switch-title">New run</span>

  <div class="kind-toggle" role="radiogroup" aria-label="Run kind">
    {#each (['plan', 'build'] as const) as k (k)}
      <button
        type="button"
        role="radio"
        aria-checked={kind === k}
        class:selected={kind === k}
        onclick={() => (kind = k)}
      >
        {k === 'plan' ? 'Plan' : 'Build'}
      </button>
    {/each}
  </div>

  <textarea
    class="task"
    placeholder="Describe the task…"
    rows="3"
    bind:value={task}
  ></textarea>

  <div class="repo-row">
    <input class="repo-input" type="text" placeholder="Target repo path" bind:value={repoPath} />
    <button class="browse-btn" type="button" onclick={browseRepo}>Browse…</button>
  </div>

  <div class="depth-toggle" role="radiogroup" aria-label="Depth">
    {#each DEPTHS as d (d.value)}
      <button
        type="button"
        role="radio"
        aria-checked={depth === d.value}
        class:selected={depth === d.value}
        onclick={() => (depth = d.value)}
      >
        {d.label}
      </button>
    {/each}
  </div>

  {#if error}<div class="status-error">⚠️ {error}</div>{/if}

  <button class="launch-btn" disabled={!canLaunch} onclick={launch}>
    {launching ? 'Launching…' : 'Launch'}
  </button>
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 10px; }
  .switch-title { font-size: 13px; color: #e0dff5; font-weight: 600; }

  .kind-toggle, .depth-toggle {
    display: inline-flex;
    gap: 2px;
    align-self: flex-start;
    border: 1px solid #2e2e4a;
    border-radius: 6px;
    overflow: hidden;
  }
  .kind-toggle button, .depth-toggle button {
    background: transparent;
    border: none;
    padding: 4px 12px;
    min-height: 28px;
    cursor: pointer;
    color: #a09ac0;
    font-size: 12px;
  }
  .kind-toggle button.selected, .depth-toggle button.selected {
    background: #7c3aed;
    color: #fff;
  }

  .task {
    resize: vertical;
    min-height: 60px;
    padding: 8px 10px;
    border-radius: 8px;
    border: 1px solid #2e2e4a;
    background: #0f0f18;
    color: #ddd8f5;
    font-size: 12px;
    font-family: inherit;
  }

  .repo-row { display: flex; gap: 6px; }
  .repo-input {
    flex: 1;
    min-width: 0;
    padding: 6px 10px;
    min-height: 32px;
    border-radius: 8px;
    border: 1px solid #2e2e4a;
    background: #0f0f18;
    color: #ddd8f5;
    font-size: 12px;
  }
  .browse-btn, .launch-btn {
    flex-shrink: 0;
    padding: 6px 14px;
    min-height: 32px;
    border-radius: 8px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #a09ac0;
    font-size: 12px;
    cursor: pointer;
  }
  .browse-btn:hover { border-color: #4b4b6a; color: #e0dff5; }

  .launch-btn {
    align-self: flex-start;
    background: #7c3aed;
    border-color: #7c3aed;
    color: #fff;
    font-weight: 600;
  }
  .launch-btn:disabled { opacity: 0.5; cursor: default; }

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
