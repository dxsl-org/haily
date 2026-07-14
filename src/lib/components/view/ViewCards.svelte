<script lang="ts">
  // Cards projection: one card per record (View Engine Phase A). `DataView` carries no
  // explicit "title field" concept (see `crates/haily-types/src/lib.rs::DataView` — schema/
  // records/projections only), so the card title always falls back to the first `Text`-type
  // field's value; if none exists, the first field of any type is used instead.
  import type { DataView, FieldDef } from '$lib/tauri';
  import { formatCellValue } from '$lib/data-view';
  import ViewCell from './ViewCell.svelte';

  let { view }: { view: DataView } = $props();

  const titleField: FieldDef | null = $derived(
    view.schema.find((f) => f.ftype.type === 'Text') ?? view.schema[0] ?? null,
  );

  const bodyFields = $derived(
    view.schema.filter((f) => f.name !== titleField?.name),
  );

  function titleText(record: Record<string, unknown>): string {
    if (!titleField) return view.entity;
    const formatted = formatCellValue(record[titleField.name], titleField.ftype);
    return formatted || view.entity;
  }
</script>

<div class="cards">
  {#each view.records as record, i (i)}
    <div class="card">
      <div class="card-title">{titleText(record)}</div>
      <div class="card-body">
        {#each bodyFields as field (field.name)}
          <div class="field-row">
            <span class="field-label">{field.label}</span>
            <ViewCell value={record[field.name]} {field} />
          </div>
        {/each}
      </div>
    </div>
  {:else}
    <div class="empty">Không có dữ liệu.</div>
  {/each}
</div>

<style>
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
    gap: 10px;
  }

  .card {
    background: #16162a;
    border: 1px solid #23233a;
    border-radius: 10px;
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    min-width: 0;
  }

  .card-title {
    font-size: 13px;
    font-weight: 600;
    color: #e0dff5;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .card-body {
    display: flex;
    flex-direction: column;
    gap: 5px;
  }

  .field-row {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 8px;
    font-size: 12px;
  }

  .field-label {
    color: #6b6b8a;
    flex-shrink: 0;
  }

  .empty {
    grid-column: 1 / -1;
    text-align: center;
    color: #6b6b8a;
    padding: 16px;
    border: 1px dashed #2e2e4a;
    border-radius: 8px;
  }
</style>
