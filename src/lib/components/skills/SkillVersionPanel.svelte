<script lang="ts">
  // Version history + revert (D4). `listSkillVersions` returns newest-first; each save (and each
  // revert itself, which snapshots the pre-revert content first) appends one row — see
  // `haily_kms::skill_editor::ops::snapshot_current`'s doc comment for why this is also the
  // crash-safety mechanism, not just a UI convenience.
  import { listSkillVersions, revertSkill, type SkillEditKind, type SkillVersion } from '$lib/tauri';

  let { name, kind, onReverted }: { name: string; kind: SkillEditKind; onReverted: () => void } = $props();

  let versions = $state<SkillVersion[]>([]);
  let loading = $state(true);
  let error = $state('');
  let revertingId = $state<string | null>(null);
  let expandedId = $state<string | null>(null);

  $effect(() => {
    load();
  });

  async function load() {
    loading = true;
    error = '';
    try {
      versions = await listSkillVersions(name);
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  function toggleExpand(id: string) {
    expandedId = expandedId === id ? null : id;
  }

  async function doRevert(v: SkillVersion) {
    if (revertingId) return;
    if (
      !confirm(
        `Khôi phục kỹ năng "${name}" về phiên bản lúc ${v.created_at}? Nội dung hiện tại sẽ được lưu ` +
          'lại thành một phiên bản mới trước khi khôi phục, nên vẫn có thể quay lại được.',
      )
    ) {
      return;
    }
    revertingId = v.id;
    error = '';
    try {
      await revertSkill(name, v.id);
      onReverted();
      await load();
    } catch (e) {
      error = String(e);
    } finally {
      revertingId = null;
    }
  }
</script>

<div class="version-panel">
  <div class="panel-header">
    <span class="title">Lịch sử phiên bản</span>
    <button class="refresh-btn" onclick={load} disabled={loading} title="Làm mới">↻</button>
  </div>

  {#if loading}
    <div class="empty">Đang tải…</div>
  {:else if error}
    <div class="status-error">⚠️ {error}</div>
  {:else if versions.length === 0}
    <div class="empty">Chưa có phiên bản nào được lưu.</div>
  {:else}
    <div class="rows">
      {#each versions as v (v.id)}
        <div class="version-row">
          <div class="version-head">
            <span class="date">{v.created_at}</span>
            {#if v.note}<span class="note">{v.note}</span>{/if}
          </div>
          <div class="version-actions">
            <button class="expand-btn" onclick={() => toggleExpand(v.id)}>
              {expandedId === v.id ? 'Ẩn nội dung' : 'Xem nội dung'}
            </button>
            <button class="revert-btn" onclick={() => doRevert(v)} disabled={revertingId !== null}>
              {revertingId === v.id ? 'Đang khôi phục…' : 'Khôi phục'}
            </button>
          </div>
          {#if expandedId === v.id}
            <pre class="content-preview">{v.content_md}</pre>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .version-panel { display: flex; flex-direction: column; gap: 8px; margin-top: 14px; }

  .panel-header { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
  .title { font-size: 12px; color: #e0dff5; font-weight: 600; }

  .refresh-btn {
    width: 28px;
    padding: 3px 0;
    text-align: center;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
    background: #16162a;
    color: #8884aa;
    font-size: 12px;
    cursor: pointer;
  }
  .refresh-btn:disabled { opacity: 0.5; cursor: default; }

  .rows { display: flex; flex-direction: column; gap: 6px; }

  .version-row {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px;
    background: #0f0f18;
    border: 1px solid #23233a;
    border-radius: 7px;
  }
  .version-head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  .date { font-size: 11px; color: #a09ac0; }
  .note { font-size: 10px; color: #6b6b8a; font-style: italic; }

  .version-actions { display: flex; gap: 6px; }
  .expand-btn, .revert-btn {
    padding: 4px 10px;
    min-height: 28px;
    border-radius: 7px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #8884aa;
    font-size: 11px;
    cursor: pointer;
  }
  .revert-btn { color: #c084fc; border-color: #4a3a7a; }
  .revert-btn:disabled { opacity: 0.5; cursor: default; }

  .content-preview {
    max-height: 200px;
    overflow-y: auto;
    white-space: pre-wrap;
    font-size: 10px;
    color: #8884aa;
    padding: 8px;
    background: #0a0a12;
    border-radius: 6px;
    border: 1px solid #23233a;
  }

  .empty { font-size: 11px; color: #6b6b8a; }
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
