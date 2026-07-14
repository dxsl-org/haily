<script lang="ts">
  // Projection dispatch (View Engine Phase A): `Table`→`ViewTable`, `Cards`→`ViewCards`,
  // anything else (`Kanban`/`Calendar`/`Chart`/unrecognized) → `ViewTable` — the wire-compat
  // fallback contract on `haily_types::ProjectionKind`. `normalizeProjectionKind` (not a
  // second switch here) is the single source of truth for that fallback.
  import type { DataView } from '$lib/tauri';
  import { normalizeProjectionKind } from '$lib/data-view';
  import ViewTable from './ViewTable.svelte';
  import ViewCards from './ViewCards.svelte';

  let { view }: { view: DataView } = $props();

  const renderKind = $derived(normalizeProjectionKind(view.active.kind));
</script>

{#if renderKind === 'Cards'}
  <ViewCards {view} />
{:else}
  <ViewTable {view} />
{/if}
