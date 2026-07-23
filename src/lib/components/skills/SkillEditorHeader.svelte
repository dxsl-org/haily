<script lang="ts">
  // Name + kind/recovered badges for `SkillEditor.svelte` — split out purely to keep that file
  // under the 200-line convention (no reuse elsewhere).
  import type { SkillEditKind } from '$lib/tauri';

  let { name, kind, recovered }: { name: string; kind: SkillEditKind; recovered: boolean } = $props();
</script>

<div class="header">
  <h2>{name}</h2>
  <span class="badge kind-{kind}">{kind === 'authored' ? 'Chính thức' : 'Học được'}</span>
  {#if recovered}<span class="badge recovered">⚠ khôi phục từ bản lưu trước</span>{/if}
</div>

<style>
  .header { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  h2 { margin: 0; font-size: 15px; color: #e0dff5; }

  .badge {
    font-size: 10px;
    padding: 2px 8px;
    border-radius: 999px;
    background: #1e1e35;
    border: 1px solid #2e2e4a;
    color: #a09ac0;
  }
  .badge.kind-authored { color: #60a5fa; }
  .badge.kind-synthesized { color: #c084fc; }
  .badge.recovered { color: #fbbf24; border-color: #7f5a1d; }
</style>
