<script lang="ts">
  // Phase 6 surface: the kill switch (pause all writes) + data export. The recent-actions/
  // undo list that used to live here was extracted into its own Settings tab
  // (`JournalBrowser.svelte`, mounted from `Settings.svelte`) — this tab stays
  // safety-toggle-only. Plain, non-technical copy throughout — no "RiskTier"/"kill
  // switch"/"compensation" jargon in anything the user reads (the code comments keep
  // those terms for devs).
  import { exportDatabase } from '$lib/tauri';
  import { save as saveFileDialog } from '@tauri-apps/plugin-dialog';

  let {
    prefs,
    save,
  }: {
    prefs: Record<string, string>;
    save: (key: string, value: string) => Promise<void>;
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
