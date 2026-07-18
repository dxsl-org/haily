<script lang="ts">
  // Renders ONE record field value by its `FieldType` (View Engine Phase A). Every value is
  // MODEL-AUTHORED (`LlmProjected` provenance) or registry data — treated as untrusted display
  // text throughout. Every branch below binds via `{expression}`; `{@html}` is FORBIDDEN in
  // this directory (SEC F1, grep-gate enforced). `Url`/`Email`/`Phone` render as inert text
  // UNLESS `safeHref` (the sole approved allowlist gate) approves a clickable `href=` — never
  // bind the raw field value into `href=` directly.
  import type { FieldDef } from '$lib/tauri';
  import { formatCellValue, safeHref } from '$lib/data-view';

  let { value, field }: { value: unknown; field: FieldDef } = $props();

  const text = $derived(formatCellValue(value, field.ftype));

  const linkHref = $derived.by(() => {
    if (typeof value !== 'string') return null;
    if (field.ftype.type === 'Url') return safeHref(value, 'url');
    if (field.ftype.type === 'Email') return safeHref(value, 'email');
    if (field.ftype.type === 'Phone') return safeHref(value, 'phone');
    return null;
  });
</script>

{#if linkHref}
  <a class="cell-link" href={linkHref} target="_blank" rel="noopener noreferrer">{text}</a>
{:else}
  <span class="cell-text">{text}</span>
{/if}

<style>
  .cell-text {
    display: inline-block;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: #ddd8f5;
  }

  .cell-link {
    color: #c084fc;
    text-decoration: underline;
    text-underline-offset: 2px;
  }
  .cell-link:hover {
    color: #a855f7;
  }
</style>
