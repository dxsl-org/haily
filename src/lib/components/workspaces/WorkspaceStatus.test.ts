import { describe, expect, it } from 'vitest';
import {
  ROW_COPY,
  workspaceStatusHint,
  workspaceStatusLabel,
  workspaceTaskLabel,
  type RunStatusRaw,
  type WorkspaceStatusLabel,
} from './WorkspaceStatus';

const ALL_LABELS: WorkspaceStatusLabel[] = ['đang chạy', 'chờ áp dụng', 'đã áp dụng', 'đã dọn dẹp'];
const SAMPLE_RUN_STATUSES: RunStatusRaw[] = [
  null,
  'queued',
  'running',
  'paused',
  'interrupted',
  'done',
  'failed',
];
const GIT_TERMS = ['worktree', 'branch', 'git', 'HEAD', 'commit'];

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
      for (const runStatus of SAMPLE_RUN_STATUSES) {
        const hint = workspaceStatusHint(label, runStatus);
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
    expect(workspaceStatusHint('đã áp dụng', null)).toContain('tự động được dọn dẹp');
    expect(workspaceStatusHint('chờ áp dụng', null)).toContain('tự động được dọn dẹp');
    expect(workspaceStatusHint('đã áp dụng', 'done')).not.toContain('tự động được dọn dẹp');
  });

  it('review LOW-3: disambiguates a stopped interrupted/failed run from a genuine apply', () => {
    // A clean tree after `interrupted`/`failed` must never read as if a change actually landed.
    const interruptedHint = workspaceStatusHint('đã áp dụng', 'interrupted');
    const failedHint = workspaceStatusHint('đã áp dụng', 'failed');
    expect(interruptedHint).not.toBe(workspaceStatusHint('đã áp dụng', 'done'));
    expect(failedHint).toBe(interruptedHint);
    expect(interruptedHint.toLowerCase()).toContain('tạm dừng');
  });

  it('a genuinely done run with a clean tree keeps the plain "no changes pending" hint', () => {
    expect(workspaceStatusHint('đã áp dụng', 'done')).toBe('Không có thay đổi nào đang chờ.');
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

describe('ROW_COPY (review LOW-4: cover the actual rendered row/button copy, not just labels/hints)', () => {
  it('never contains a git term in any static row/button string WorkspaceRow renders', () => {
    for (const [key, value] of Object.entries(ROW_COPY)) {
      for (const term of GIT_TERMS) {
        expect(value.toLowerCase(), `ROW_COPY.${key} must not contain "${term}"`).not.toContain(
          term.toLowerCase(),
        );
      }
    }
  });

  it('never promises a specific approval action it cannot honestly back (review MED)', () => {
    // The pending-approval notice must stay generic/session-scoped, never claim to be the
    // diff-apply request specifically — no "Áp dụng"/"Từ chối" wording here.
    expect(ROW_COPY.pendingApprovalNotice).not.toContain('Áp dụng');
    expect(ROW_COPY.pendingApprovalNotice).not.toContain('Từ chối');
  });
});
