import { describe, expect, it } from 'vitest';
import { filterCommands, groupBySource, groupLabel, flattenGroups } from './palette-filter';
import type { SlashCommand } from './tauri';

function cmd(overrides: Partial<SlashCommand> & Pick<SlashCommand, 'name' | 'source'>): SlashCommand {
  return {
    description: '',
    arg_hint: null,
    example: null,
    action: { type: 'BuiltIn', data: 'pass_through' },
    ...overrides,
  };
}

const registry: SlashCommand[] = [
  cmd({ name: 'plan', source: 'built_in', description: 'Lập kế hoạch một tính năng' }),
  cmd({ name: 'build', source: 'built_in', description: 'Xây dựng và kiểm thử' }),
  cmd({ name: 'review-notes', source: 'authored', description: 'Tóm tắt ghi chú cần xem lại' }),
  cmd({ name: 'weekly-digest', source: 'synthesized', description: 'Tổng hợp việc trong tuần' }),
];

describe('filterCommands', () => {
  it('returns everything for an empty/blank query', () => {
    expect(filterCommands(registry, '')).toHaveLength(4);
    expect(filterCommands(registry, '   ')).toHaveLength(4);
  });

  it('matches by name substring, case-insensitively', () => {
    const result = filterCommands(registry, 'PL');
    expect(result.map((c) => c.name)).toEqual(['plan']);
  });

  it('matches by description substring when the name does not match', () => {
    const result = filterCommands(registry, 'tuần');
    expect(result.map((c) => c.name)).toEqual(['weekly-digest']);
  });

  it('returns an empty array when nothing matches', () => {
    expect(filterCommands(registry, 'zzz-no-match')).toEqual([]);
  });
});

describe('groupBySource / flattenGroups', () => {
  it('groups in built_in > authored > synthesized order, dropping empty groups', () => {
    const groups = groupBySource(registry);
    expect(groups.map((g) => g.source)).toEqual(['built_in', 'authored', 'synthesized']);
    expect(groups[0].items.map((c) => c.name)).toEqual(['plan', 'build']);
  });

  it('omits a group entirely when it has no members', () => {
    const groups = groupBySource(registry.filter((c) => c.source !== 'authored'));
    expect(groups.map((g) => g.source)).toEqual(['built_in', 'synthesized']);
  });

  it('dedupes by name defensively — a duplicate model-authored name never appears twice', () => {
    const withDup: SlashCommand[] = [
      ...registry,
      cmd({ name: 'weekly-digest', source: 'synthesized', description: 'a stray duplicate row' }),
    ];
    const groups = groupBySource(withDup);
    const flat = flattenGroups(groups);
    expect(flat.filter((c) => c.name === 'weekly-digest')).toHaveLength(1);
  });

  it('flattenGroups preserves the same render order used for keyboard index math', () => {
    const flat = flattenGroups(groupBySource(registry));
    expect(flat.map((c) => c.name)).toEqual(['plan', 'build', 'review-notes', 'weekly-digest']);
  });
});

describe('groupLabel', () => {
  it('returns a distinct Vietnamese label for every source', () => {
    const labels = new Set((['built_in', 'authored', 'synthesized'] as const).map(groupLabel));
    expect(labels.size).toBe(3);
  });
});
