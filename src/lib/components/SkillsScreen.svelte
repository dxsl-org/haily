<script lang="ts">
  // Skills destination (P01 shell, filled by Unified Chat UI phase 9, D4): the list tab
  // (`SkillsBrowser`, relocated here) opens the structured editor (`skills/SkillEditor`) on a
  // row click; `selected` is the only state this composition owns.
  import SkillsBrowser from './SkillsBrowser.svelte';
  import SkillEditor from './skills/SkillEditor.svelte';
  import type { SkillEditKind } from '$lib/tauri';

  let selected = $state<{ name: string; kind: SkillEditKind } | null>(null);

  function openSkill(name: string, kind: SkillEditKind) {
    selected = { name, kind };
  }

  function back() {
    selected = null;
  }
</script>

<div class="screen">
  {#if selected}
    <SkillEditor name={selected.name} kind={selected.kind} onBack={back} />
  {:else}
    <SkillsBrowser onOpenSkill={openSkill} />
  {/if}
</div>

<style>
  .screen {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 4px 2px;
  }
</style>
