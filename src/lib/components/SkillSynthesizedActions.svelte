<script lang="ts">
  // Manual Archive + Promote-to-authored for a synthesized skill row (D4, phase 9 step 4).
  // Split out of `SkillRow.svelte` to keep that file under the 200-line convention — a row-level
  // action group, not part of the `skills/` editor surface (that's `SkillEditor` and friends).
  import { archiveSkillManual, promoteSkill } from '$lib/tauri';

  let { name, onChanged }: { name: string; onChanged: () => void } = $props();

  let archiving = $state(false);
  let promoting = $state(false);
  let error = $state('');

  async function archive() {
    if (archiving || promoting) return;
    if (!confirm(`Lưu trữ kỹ năng "${name}"? Kỹ năng sẽ ngừng được sử dụng nhưng lịch sử vẫn được giữ lại.`)) return;
    archiving = true;
    error = '';
    try {
      await archiveSkillManual(name);
      onChanged();
    } catch (e) {
      error = String(e);
    } finally {
      archiving = false;
    }
  }

  async function promote() {
    if (archiving || promoting) return;
    if (
      !confirm(
        `Chuyển "${name}" thành kỹ năng chính thức (authored)? Kỹ năng sẽ không còn tự động giảm ` +
          'độ tin cậy theo thời gian nữa. Không thể hoàn tác thao tác này.',
      )
    )
      return;
    promoting = true;
    error = '';
    try {
      await promoteSkill(name);
      onChanged();
    } catch (e) {
      error = String(e);
    } finally {
      promoting = false;
    }
  }
</script>

<div class="synth-actions">
  <button class="archive-btn" onclick={archive} disabled={archiving || promoting}>
    {archiving ? 'Đang lưu trữ…' : 'Lưu trữ'}
  </button>
  <button class="promote-btn" onclick={promote} disabled={archiving || promoting}>
    {promoting ? 'Đang chuyển…' : 'Chuyển thành chính thức'}
  </button>
  {#if error}<div class="status-error">⚠️ {error}</div>{/if}
</div>

<style>
  .synth-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }

  .archive-btn, .promote-btn {
    padding: 4px 10px;
    min-height: 28px;
    border-radius: 999px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #8884aa;
    font-size: 11px;
    cursor: pointer;
  }
  .archive-btn:hover:not(:disabled) { color: #fbbf24; border-color: #7f5a1d; }
  .promote-btn:hover:not(:disabled) { color: #c084fc; border-color: #4a3a7a; }
  .archive-btn:disabled, .promote-btn:disabled { opacity: 0.5; cursor: default; }

  .status-error {
    font-size: 11px;
    padding: 4px 8px;
    border-radius: 6px;
    background: #2a0f0f;
    color: #f87171;
    border: 1px solid #7f1d1d;
    word-break: break-word;
  }
</style>
