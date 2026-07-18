<script lang="ts">
  // Client-side projection switcher (View Engine Phase A) — the parent already holds every
  // projection's data in the fetched `DataView`, so switching never refetches; this component
  // only picks which one is active and reports the choice upward for telemetry.
  import type { ProjectionSpec } from '$lib/tauri';
  import { normalizeProjectionKind, projectionLabel } from '$lib/data-view';

  let {
    projections,
    active,
    onSwitch,
  }: {
    projections: ProjectionSpec[];
    active: ProjectionSpec;
    onSwitch: (spec: ProjectionSpec) => void;
  } = $props();
</script>

{#if projections.length > 1}
  <div class="switcher" role="group" aria-label="Chuyển kiểu hiển thị">
    {#each projections as spec (spec.kind)}
      <button
        class="switch-btn"
        class:active={normalizeProjectionKind(active.kind) === normalizeProjectionKind(spec.kind)}
        onclick={() => onSwitch(spec)}
      >{projectionLabel(spec.kind)}</button>
    {/each}
  </div>
{/if}

<style>
  .switcher {
    display: flex;
    gap: 4px;
  }

  .switch-btn {
    padding: 4px 10px;
    min-height: 28px;
    border-radius: 7px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #8884aa;
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
  }
  .switch-btn.active { background: #2a2a45; color: #c084fc; border-color: #4a3a7a; }
  .switch-btn:hover:not(.active) { color: #a09ac0; }
</style>
