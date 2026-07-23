<script lang="ts">
  // Dropdown/menu shared by both palette entry paths (D6): typing "/" at the start of the
  // chat input, and the ＋ button. Owns fetch + filter + keyboard nav — the caller
  // (`ChatInput.svelte`) only decides WHEN to show it and what to do with a selection
  // (phase-03 architecture: "SlashPalette owns fetch + filter + keyboard").
  import { listSlashCommands, type SlashCommand } from '$lib/tauri';
  import { filterCommands, groupBySource, flattenGroups, groupLabel } from '$lib/palette-filter';

  interface Props {
    open: boolean;
    /** Typed token after "/" (inline path) or "" for the ＋ menu — narrows the list. */
    filter: string;
    onSelect: (name: string) => void;
    onClose: () => void;
  }

  let { open, filter, onSelect, onClose }: Props = $props();

  let commands = $state<SlashCommand[]>([]);
  let loading = $state(false);
  let error = $state('');
  let selectedIndex = $state(0);

  // Refetch every time the palette opens — cheap, in-memory snapshot server-side (P02) —
  // so a skill just enabled/edited elsewhere appears without restarting the GUI.
  $effect(() => {
    if (open) load();
  });

  async function load() {
    loading = true;
    error = '';
    try {
      commands = await listSlashCommands();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  const filtered = $derived(filterCommands(commands, filter));
  const groups = $derived(groupBySource(filtered));
  const flat = $derived(flattenGroups(groups));
  const indexByName = $derived(new Map(flat.map((c, i) => [c.name, i])));

  // Reset the highlighted row whenever the visible set could have reshaped (new filter
  // text, or the palette just (re)opened) — a surviving index could point past a now-
  // narrower list.
  $effect(() => {
    filter;
    open;
    selectedIndex = 0;
  });

  function confirm(cmd: SlashCommand | undefined) {
    if (!cmd) return;
    onSelect(cmd.name);
  }

  // Capture phase so ↑/↓/Enter/Esc/Tab are intercepted BEFORE the textarea's own keydown
  // handler runs (bubble/target phase fires after capture, regardless of stopPropagation
  // — e.g. Enter would otherwise also send the message). Attached only while `open`, so
  // normal typing elsewhere is never touched.
  $effect(() => {
    if (!open) return;
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        e.stopPropagation();
        if (flat.length) selectedIndex = (selectedIndex + 1) % flat.length;
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        e.stopPropagation();
        if (flat.length) selectedIndex = (selectedIndex - 1 + flat.length) % flat.length;
      } else if (e.key === 'Enter' || e.key === 'Tab') {
        if (!flat.length) return;
        e.preventDefault();
        e.stopPropagation();
        confirm(flat[selectedIndex]);
      } else if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener('keydown', handleKey, true);
    return () => window.removeEventListener('keydown', handleKey, true);
  });
</script>

{#if open}
  <div class="palette" role="listbox" aria-label="Danh sách lệnh">
    {#if loading}
      <div class="status">Đang tải…</div>
    {:else if error}
      <div class="status error">⚠️ {error}</div>
    {:else if flat.length === 0}
      <div class="status">Không có lệnh phù hợp.</div>
    {:else}
      {#each groups as group (group.source)}
        <div class="group-label">{groupLabel(group.source)}</div>
        {#each group.items as cmd (cmd.name)}
          {@const idx = indexByName.get(cmd.name) ?? -1}
          <button
            type="button"
            class="row"
            class:active={idx === selectedIndex}
            role="option"
            aria-selected={idx === selectedIndex}
            onmouseenter={() => (selectedIndex = idx)}
            onclick={() => confirm(cmd)}
          >
            <div class="row-main">
              <span class="name">/{cmd.name}</span>
              {#if cmd.arg_hint}<span class="hint">{cmd.arg_hint}</span>{/if}
            </div>
            <span class="desc">{cmd.description}</span>
            {#if cmd.example}<span class="example">{cmd.example}</span>{/if}
          </button>
        {/each}
      {/each}
    {/if}
  </div>
{/if}

<style>
  .palette {
    position: absolute;
    left: 12px;
    right: 12px;
    bottom: 100%;
    margin-bottom: 6px;
    max-height: 320px;
    overflow-y: auto;
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 12px;
    box-shadow: 0 12px 32px rgba(0, 0, 0, 0.45);
    padding: 6px;
    z-index: 50;
  }

  .status {
    padding: 10px 8px;
    font-size: 12px;
    color: #6b6b8a;
  }
  .status.error { color: #f87171; }

  .group-label {
    padding: 6px 8px 2px;
    font-size: 10px;
    font-weight: 700;
    letter-spacing: 0.03em;
    text-transform: uppercase;
    color: #6b6b8a;
  }

  .row {
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 100%;
    text-align: left;
    padding: 6px 8px;
    border: none;
    border-radius: 8px;
    background: transparent;
    color: #e0dff5;
    cursor: pointer;
  }
  .row:hover, .row.active { background: #2a2a45; }

  .row-main { display: flex; align-items: baseline; gap: 8px; }
  .name { font-size: 13px; font-weight: 600; color: #c084fc; }
  .hint { font-size: 11px; color: #6b6b8a; }
  .desc { font-size: 11px; color: #8884aa; line-height: 1.4; }
  .example { font-size: 10px; color: #6b6b8a; font-style: italic; }
</style>
