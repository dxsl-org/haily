<script lang="ts">
  // Phase 6 surface: the kill switch (pause all writes) and a recent-actions/undo list.
  // Plain, non-technical copy throughout — no "RiskTier"/"kill switch"/"compensation"
  // jargon in anything the user reads (the code comments keep those terms for devs).
  import { sendMessage } from '$lib/tauri';
  import { listJournal, exportDatabase, type JournalEntry } from '$lib/tauri';
  import { save as saveFileDialog } from '@tauri-apps/plugin-dialog';

  let {
    prefs,
    save,
    sessionIds,
  }: {
    prefs: Record<string, string>;
    save: (key: string, value: string) => Promise<void>;
    /** Every session id this GUI instance has started — see `+page.svelte`. There is no
     * single "current session" (each turn mints a fresh one), so the recent-actions list
     * is scoped to "this app run" rather than to one conversation. */
    sessionIds: () => string[];
  } = $props();

  // `safety.disable_writes` defaults to unset (writes enabled) until the user first
  // touches the toggle — treat anything other than the literal "true"/"1" as off.
  const writesPaused = () => prefs['safety.disable_writes'] === 'true' || prefs['safety.disable_writes'] === '1';

  let toggling = $state(false);
  async function toggleWrites() {
    if (toggling) return;
    toggling = true;
    try {
      await save('safety.disable_writes', writesPaused() ? 'false' : 'true');
    } finally {
      toggling = false;
    }
  }

  // Harness Completion phase 4 (M5a/M5b): the backend sets `credential.fallback_active`
  // as a PERSISTED row (not just a log line) whenever it had to read/write a connector
  // secret through the plaintext DB fallback instead of the OS keyring — e.g. headless/
  // Session-0 boot (M5a) or a platform keyring RPC failure (M5b). Surfaced here since this
  // tab already receives the full preference map on every open; acknowledging clears the
  // flag so a resolved one-time event doesn't keep reappearing every time Settings opens.
  const credentialFallbackActive = () => prefs['credential.fallback_active'] === 'true';

  let dismissingFallback = $state(false);
  async function dismissFallbackWarning() {
    if (dismissingFallback) return;
    dismissingFallback = true;
    try {
      await save('credential.fallback_active', 'false');
    } finally {
      dismissingFallback = false;
    }
  }

  let entries = $state<JournalEntry[]>([]);
  let loading = $state(false);
  let loadError = $state('');

  async function loadEntries() {
    loading = true;
    loadError = '';
    try {
      entries = await listJournal(sessionIds());
    } catch (e) {
      loadError = String(e);
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    loadEntries();
  });

  // Undo has no dedicated backend call (phase 6 is surface-only) — it sends a precise
  // chat instruction naming the journal id, which the LLM turns into a `journal_undo`
  // tool call through the normal approval-gated path. The tool's own reply text already
  // reports the three batch counts (undone/failed/not_attempted), so it renders in the
  // main chat like any other assistant message rather than being re-parsed here.
  let undoing = $state<string | null>(null);
  let undoError = $state('');
  async function requestUndo(id: string) {
    if (undoing) return;
    undoing = id;
    undoError = '';
    try {
      await sendMessage(`Undo the action with journal id "${id}".`);
    } catch (e) {
      undoError = String(e);
    } finally {
      undoing = null;
    }
  }

  function statusLabel(entry: JournalEntry): string {
    switch (entry.undoStatus) {
      case 'undone':
        return 'Undone';
      case 'stuck':
        return 'Stuck — needs manual action';
      case 'compensation_failed':
        return 'Undo failed — can retry';
      case 'refused':
        return 'Undo refused';
      case 'undo_requested':
      case 'compensating':
        return 'Undo in progress…';
      default:
        return 'Not undone';
    }
  }

  function canUndo(entry: JournalEntry): boolean {
    return entry.undoStatus === 'not_requested' || entry.undoStatus === 'compensation_failed';
  }

  // Phase 6 ("Activate & Measure"): `backup.age_warning_active` is a PERSISTED flag the
  // scheduled backup worker refreshes every cycle (mirrors `credential.fallback_active`'s
  // pattern) — a silently-starved backup (disk full, permissions, crash loop) must not
  // stay invisible. Dismissing here just clears the flag until the worker's next cycle;
  // it re-raises it if the underlying staleness has not actually resolved.
  const backupAgeWarningActive = () => prefs['backup.age_warning_active'] === 'true';

  let dismissingBackupWarning = $state(false);
  async function dismissBackupWarning() {
    if (dismissingBackupWarning) return;
    dismissingBackupWarning = true;
    try {
      await save('backup.age_warning_active', 'false');
    } finally {
      dismissingBackupWarning = false;
    }
  }

  let exporting = $state(false);
  let exportError = $state('');
  let exportSuccessPath = $state('');
  async function exportDb() {
    if (exporting) return;
    exportError = '';
    exportSuccessPath = '';
    try {
      const destPath = await saveFileDialog({
        title: 'Export Haily database',
        defaultPath: 'haily-export.db',
        filters: [{ name: 'SQLite database', extensions: ['db'] }],
      });
      if (!destPath) return; // user cancelled the dialog
      exporting = true;
      await exportDatabase(destPath);
      exportSuccessPath = destPath;
    } catch (e) {
      exportError = String(e);
    } finally {
      exporting = false;
    }
  }
</script>

<div class="section">
  {#if credentialFallbackActive()}
    <div class="block fallback-warning">
      <span class="warning-title">⚠️ A saved login had to use a less secure backup</span>
      <span class="hint">
        Haily couldn't reach your device's secure credential storage, so it temporarily
        used its own database instead. Your data is still local-only. This usually
        resolves itself — try again later, or check the Odoo connector setup if it
        keeps happening.
      </span>
      <button class="dismiss-btn" onclick={dismissFallbackWarning} disabled={dismissingFallback}>
        {dismissingFallback ? 'Closing…' : 'Got it'}
      </button>
    </div>
  {/if}

  {#if backupAgeWarningActive()}
    <div class="block fallback-warning">
      <span class="warning-title">⚠️ Your data hasn't been backed up recently</span>
      <span class="hint">
        Haily makes a local backup copy of your data automatically, but the last one
        didn't complete in time. Check that Haily has been able to run recently and that
        there's enough free disk space.
      </span>
      <button class="dismiss-btn" onclick={dismissBackupWarning} disabled={dismissingBackupWarning}>
        {dismissingBackupWarning ? 'Closing…' : 'Got it'}
      </button>
    </div>
  {/if}

  <div class="block">
    <div class="switch-copy">
      <span class="switch-title">Export your data</span>
      <span class="hint">
        Save a complete copy of your local database to a file of your choice. This file
        is not encrypted and contains everything Haily knows — keep it somewhere safe.
      </span>
    </div>
    <button class="undo-btn" onclick={exportDb} disabled={exporting}>
      {exporting ? 'Exporting…' : 'Export database…'}
    </button>
    {#if exportSuccessPath}
      <div class="hint">Exported to {exportSuccessPath}</div>
    {/if}
    {#if exportError}
      <div class="status-error">⚠️ {exportError}</div>
    {/if}
  </div>

  <div class="block">
    <div class="switch-row">
      <div class="switch-copy">
        <span class="switch-title">Pause all writes</span>
        <span class="hint">While this is on, Haily will ask before it can make any new changes at all.</span>
      </div>
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
    </div>
  </div>

  <div class="block">
    <div class="list-header">
      <span class="switch-title">Recent actions</span>
      <button class="refresh-btn icon" onclick={loadEntries} title="Refresh" disabled={loading}>↻</button>
    </div>

    {#if loading}
      <div class="empty">Loading…</div>
    {:else if loadError}
      <div class="status-error">⚠️ {loadError}</div>
    {:else if entries.length === 0}
      <div class="empty">Nothing to undo yet.</div>
    {:else}
      <div class="entry-list">
        {#each entries as entry (entry.id)}
          <div class="entry">
            <div class="entry-main">
              <span class="entry-tool"><code>{entry.toolName}</code></span>
              <span class="entry-status">{statusLabel(entry)}</span>
            </div>
            <div class="entry-meta">{entry.createdAt}</div>
            {#if entry.undoStatus === 'stuck'}
              <div class="stuck-plan">
                <span class="hint">This one couldn't be undone automatically. Raw plan for manual action:</span>
                <pre>{entry.compensationPlan ?? '(none recorded)'}</pre>
              </div>
            {:else if canUndo(entry)}
              <button
                class="undo-btn"
                onclick={() => requestUndo(entry.id)}
                disabled={undoing === entry.id}
              >
                {undoing === entry.id ? 'Undoing…' : 'Undo'}
              </button>
            {/if}
          </div>
        {/each}
      </div>
    {/if}

    {#if undoError}
      <div class="status-error">⚠️ {undoError}</div>
    {/if}
  </div>
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 20px; }
  .block { display: flex; flex-direction: column; gap: 10px; }

  .switch-row { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
  .switch-copy { display: flex; flex-direction: column; gap: 4px; }
  .switch-title { font-size: 13px; color: #e0dff5; font-weight: 600; }
  .hint { font-size: 11px; color: #6b6b8a; line-height: 1.5; }

  .fallback-warning {
    padding: 12px;
    background: #2a1f0f;
    border: 1px solid #7f5a1d;
    border-radius: 10px;
  }
  .warning-title { font-size: 12px; font-weight: 600; color: #fbbf24; }
  .dismiss-btn {
    align-self: flex-start;
    margin-top: 2px;
    padding: 5px 12px;
    border: 1px solid #7f5a1d;
    border-radius: 7px;
    background: #16162a;
    color: #fbbf24;
    font-size: 11px;
    cursor: pointer;
    transition: border-color 0.15s, background 0.15s;
  }
  .dismiss-btn:hover:not(:disabled) { border-color: #fbbf24; background: #1e1e35; }
  .dismiss-btn:disabled { opacity: 0.5; cursor: default; }

  .switch {
    flex-shrink: 0;
    width: 42px;
    height: 24px;
    border-radius: 999px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    cursor: pointer;
    position: relative;
    transition: background 0.15s, border-color 0.15s;
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
  .switch.on { background: #4c1d95; border-color: #7c3aed; }
  .switch.on .knob { transform: translateX(18px); background: #e0dff5; }

  .list-header { display: flex; align-items: center; justify-content: space-between; }

  .refresh-btn {
    flex-shrink: 0;
    padding: 6px 10px;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
    background: #16162a;
    color: #8884aa;
    font-size: 13px;
    cursor: pointer;
    transition: color 0.15s, border-color 0.15s;
  }
  .refresh-btn.icon { width: 30px; padding: 4px 0; text-align: center; }
  .refresh-btn:hover:not(:disabled) { color: #c084fc; border-color: #4a3a7a; }
  .refresh-btn:disabled { opacity: 0.5; cursor: default; }

  .empty {
    font-size: 12px;
    color: #6b6b8a;
    padding: 10px;
    background: #0f0f18;
    border-radius: 8px;
    border: 1px dashed #2e2e4a;
  }

  .entry-list { display: flex; flex-direction: column; gap: 8px; }
  .entry {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 10px;
    background: #0f0f18;
    border: 1px solid #23233a;
    border-radius: 8px;
  }
  .entry-main { display: flex; justify-content: space-between; align-items: baseline; gap: 8px; }
  .entry-tool code { color: #a09ac0; font-size: 12px; }
  .entry-status { font-size: 11px; color: #8884aa; }
  .entry-meta { font-size: 10px; color: #4a4a6a; }

  .stuck-plan { display: flex; flex-direction: column; gap: 4px; margin-top: 4px; }
  .stuck-plan pre {
    background: #16162a;
    border: 1px solid #2a2a45;
    border-radius: 6px;
    padding: 8px;
    font-size: 11px;
    color: #f87171;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 120px;
    overflow: auto;
  }

  .undo-btn {
    align-self: flex-start;
    margin-top: 4px;
    padding: 5px 12px;
    border: 1px solid #2e2e4a;
    border-radius: 7px;
    background: #16162a;
    color: #c084fc;
    font-size: 11px;
    cursor: pointer;
    transition: border-color 0.15s, background 0.15s;
  }
  .undo-btn:hover:not(:disabled) { border-color: #7c3aed; background: #1e1e35; }
  .undo-btn:disabled { opacity: 0.5; cursor: default; }

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
