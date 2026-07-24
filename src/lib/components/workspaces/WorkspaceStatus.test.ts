import { describe, expect, it } from 'vitest';
import {
  workspaceStatusHint,
  workspaceStatusLabel,
  workspaceTaskLabel,
  type WorkspaceStatusLabel,
} from './WorkspaceStatus';

const ALL_LABELS: WorkspaceStatusLabel[] = ['đang chạy', 'chờ áp dụng', 'đã áp dụng', 'đã dọn dẹp'];
const GIT_TERMS = ['worktree', 'branch', 'git', 'HEAD'];

describe('workspaceStatusLabel', () => {
  it('reads as running for a queued or running linked run', () => {
    expect(workspaceStatusLabel({ runStatus: 'queued', dirty: false, reclaimed: false })).toBe('đang chạy');
    expect(workspaceStatusLabel({ runStatus: 'running', dirty: true, reclaimed: false })).toBe('đang chạy');
  });

  it('reads as cleaned-up when the worktree is reclaimed, regardless of run status', () => {
    expect(workspaceStatusLabel({ runStatus: 'interrupted', dirty: true, reclaimed: true })).toBe('đã dọn dẹp');
    expect(workspaceStatusLabel({ runStatus: null, dirty: false, reclaimed: true })).toBe('đã dọn dẹp');
  });

  it('reads as waiting-to-apply when dirty and no live run', () => {
    expect(workspaceStatusLabel({ runStatus: 'interrupted', dirty: true, reclaimed: false })).toBe('chờ áp dụng');
    expect(workspaceStatusLabel({ runStatus: 'done', dirty: true, reclaimed: false })).toBe('chờ áp dụng');
    expect(workspaceStatusLabel({ runStatus: null, dirty: true, reclaimed: false })).toBe('chờ áp dụng');
  });

  it('reads as applied/clean when not dirty and not reclaimed', () => {
    expect(workspaceStatusLabel({ runStatus: 'done', dirty: false, reclaimed: false })).toBe('đã áp dụng');
    expect(workspaceStatusLabel({ runStatus: null, dirty: false, reclaimed: false })).toBe('đã áp dụng');
  });

  it('never emits a git term in any status label or hint', () => {
    for (const label of ALL_LABELS) {
      for (const hasRun of [true, false]) {
        const hint = workspaceStatusHint(label, hasRun);
        for (const term of GIT_TERMS) {
          expect(label.toLowerCase()).not.toContain(term.toLowerCase());
          expect(hint.toLowerCase()).not.toContain(term.toLowerCase());
        }
      }
    }
  });
});

describe('workspaceStatusHint', () => {
  it('surfaces the passive orphan note when no run is linked at all, without a new label', () => {
    expect(workspaceStatusHint('đã áp dụng', false)).toContain('tự động được dọn dẹp');
    expect(workspaceStatusHint('chờ áp dụng', false)).toContain('tự động được dọn dẹp');
    expect(workspaceStatusHint('đã áp dụng', true)).not.toContain('tự động được dọn dẹp');
  });
});

describe('workspaceTaskLabel', () => {
  it('returns the trimmed task when present', () => {
    expect(workspaceTaskLabel('  fix the login bug  ')).toBe('fix the login bug');
  });

  it('falls back to a generic label for null/blank task text', () => {
    expect(workspaceTaskLabel(null)).toBe('một tác vụ');
    expect(workspaceTaskLabel('   ')).toBe('một tác vụ');
  });
});
