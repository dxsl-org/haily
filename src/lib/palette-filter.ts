// Pure filter/group helpers for the slash command palette (P03) — kept out of
// `SlashPalette.svelte` so the matching/grouping logic is unit-testable without mounting
// a component, and to keep the component itself under the 200-line file-size convention.
import type { SlashCommand } from './tauri';

/**
 * Case-insensitive substring match over name + description. Deliberately no fuzzy-search
 * library (KISS, phase-03 spec) — the registry is small and already name-sorted
 * server-side, so a plain substring filter over both fields is enough to be useful.
 */
export function filterCommands(commands: SlashCommand[], query: string): SlashCommand[] {
  const q = query.trim().toLowerCase();
  if (!q) return commands;
  return commands.filter(
    (c) => c.name.toLowerCase().includes(q) || c.description.toLowerCase().includes(q),
  );
}

/** VN group labels for the palette/＋ menu, keyed by `SlashCommand.source`. */
const GROUP_LABELS: Record<SlashCommand['source'], string> = {
  built_in: 'Lệnh có sẵn',
  authored: 'Kỹ năng đã soạn',
  synthesized: 'Kỹ năng đã học',
};

export function groupLabel(source: SlashCommand['source']): string {
  return GROUP_LABELS[source];
}

/** Rendering order for groups: built-in first (most predictable/trusted), then authored,
 * then synthesized (model-distilled — least predictable). */
const GROUP_ORDER: SlashCommand['source'][] = ['built_in', 'authored', 'synthesized'];

export interface CommandGroup {
  source: SlashCommand['source'];
  items: SlashCommand[];
}

/**
 * Group already-filtered commands by source in `GROUP_ORDER`, dropping empty groups.
 * Defensively dedupes by `name` — command names originate from skill data (LLM/user-
 * authored) and this list's rows are keyed by name for Svelte's keyed `{#each}`; a
 * duplicate name previously caused two separate GUI crashes elsewhere (view projections,
 * view schema) when a model-authored array was trusted as unique. Guard here rather than
 * trust the registry.
 */
export function groupBySource(commands: SlashCommand[]): CommandGroup[] {
  const seenNames = new Set<string>();
  const bySource = new Map<SlashCommand['source'], SlashCommand[]>();
  for (const cmd of commands) {
    if (seenNames.has(cmd.name)) continue;
    seenNames.add(cmd.name);
    const bucket = bySource.get(cmd.source);
    if (bucket) bucket.push(cmd);
    else bySource.set(cmd.source, [cmd]);
  }
  return GROUP_ORDER.filter((s) => bySource.has(s)).map((source) => ({
    source,
    items: bySource.get(source)!,
  }));
}

/** Flatten groups back into render order — used for ↑/↓ index math over the grouped list. */
export function flattenGroups(groups: CommandGroup[]): SlashCommand[] {
  return groups.flatMap((g) => g.items);
}

/**
 * What Enter/Tab should do in the palette: confirm the highlighted row when there is one,
 * otherwise close the palette WITHOUT consuming the keystroke — the caller (`ChatInput`)
 * then applies its own normal handling for that key (e.g. Enter still sends the message
 * when a typed "/xyz" matched nothing, rather than being silently swallowed).
 */
export function confirmOrClose(hasMatches: boolean): 'confirm' | 'close' {
  return hasMatches ? 'confirm' : 'close';
}
