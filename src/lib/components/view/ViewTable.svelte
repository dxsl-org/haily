<script lang="ts">
  // Table projection: one column per `schema` field, one row per `records` entry (View Engine
  // Phase A). Every cell renders via `ViewCell` — no formatting logic lives here.
  import type { DataView } from '$lib/tauri';
  import ViewCell from './ViewCell.svelte';

  let { view }: { view: DataView } = $props();
</script>

<div class="table-wrap">
  <table>
    <thead>
      <tr>
        {#each view.schema as field (field.name)}
          <th title={field.help ?? undefined}>{field.label}</th>
        {/each}
      </tr>
    </thead>
    <tbody>
      {#each view.records as record, i (i)}
        <tr>
          {#each view.schema as field (field.name)}
            <td><ViewCell value={record[field.name]} {field} /></td>
          {/each}
        </tr>
      {:else}
        <tr><td class="empty" colspan={Math.max(view.schema.length, 1)}>Không có dữ liệu.</td></tr>
      {/each}
    </tbody>
  </table>
</div>

<style>
  .table-wrap {
    overflow: auto;
    border: 1px solid #23233a;
    border-radius: 8px;
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
  }

  th {
    position: sticky;
    top: 0;
    text-align: left;
    padding: 8px 10px;
    background: #16162a;
    color: #a8a3c9;
    font-weight: 600;
    border-bottom: 1px solid #2e2e4a;
    white-space: nowrap;
  }

  td {
    padding: 7px 10px;
    border-bottom: 1px solid #1e1e2e;
    max-width: 260px;
  }

  tr:last-child td {
    border-bottom: none;
  }

  .empty {
    text-align: center;
    color: #6b6b8a;
    padding: 16px;
  }
</style>
