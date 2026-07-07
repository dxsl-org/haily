<script lang="ts">
  // Phase 6: dedicated journal/audit browser, extracted out of SafetyTab's flat
  // "Recent actions" list (which stays safety-toggle-only now). Read-only backend
  // (`listJournal` → `journal::list_by_session`, unchanged). Per-row rendering + undo
  // lives in `JournalEntryRow.svelte` — this file owns fetch + status filtering only.
  import { listJournal, type JournalEntry } from '$lib/tauri';
  import JournalEntryRow from './JournalEntryRow.svelte';

  let { sessionIds }: { sessionIds: () => string[] } = $props();

  type StatusFilter = 'all' | 'open' | 'undone' | 'stuck' | 'failed';
  const FILTERS: { id: StatusFilter; label: string }[] = [
    { id: 'all', label: 'Tất cả' },
    { id: 'open', label: 'Chưa hoàn tác' },
    { id: 'undone', label: 'Đã hoàn tác' },
    { id: 'failed', label: 'Thất bại' },
    { id: 'stuck', label: 'Kẹt' },
  ];

  let filter = $state<StatusFilter>('all');
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

  function matchesFilter(entry: JournalEntry): boolean {
    switch (filter) {
      case 'open':
        return entry.undoStatus === 'not_requested';
      case 'undone':
        return entry.undoStatus === 'undone';
      case 'stuck':
        return entry.undoStatus === 'stuck';
      case 'failed':
        return entry.undoStatus === 'compensation_failed' || entry.undoStatus === 'refused';
      default:
        return true;
    }
  }
  const filtered = $derived(entries.filter(matchesFilter));
</script>

<div class="section">
  <div class="list-header">
    <span class="switch-title">Nhật ký hành động</span>
    <div class="header-actions">
      <select class="filter" bind:value={filter} aria-label="Lọc theo trạng thái">
        {#each FILTERS as f (f.id)}
          <option value={f.id}>{f.label}</option>
        {/each}
      </select>
      <button class="refresh-btn icon" onclick={loadEntries} title="Làm mới" disabled={loading}>↻</button>
    </div>
  </div>

  {#if loading}
    <div class="empty">Đang tải…</div>
  {:else if loadError}
    <div class="status-error">⚠️ {loadError}</div>
  {:else if filtered.length === 0}
    <div class="empty">Không có mục nào phù hợp.</div>
  {:else}
    <div class="entry-list">
      {#each filtered as entry (entry.id)}
        <JournalEntryRow {entry} />
      {/each}
    </div>
  {/if}
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 12px; }

  .list-header { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
  .header-actions { display: flex; align-items: center; gap: 6px; }
  .switch-title { font-size: 13px; color: #e0dff5; font-weight: 600; }

  .filter {
    padding: 5px 8px;
    border: 1px solid #2e2e4a;
    border-radius: 7px;
    background: #16162a;
    color: #a09ac0;
    font-size: 11px;
  }

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
