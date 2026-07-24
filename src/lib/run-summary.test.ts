import { describe, expect, it } from 'vitest';
import { applyRunEvent, type Job } from './run-events';
import { narrateRunStatus, runNeedsYou, runStatusBadge, runTaskLabel, toRowView } from './run-summary';
import type { RunEvent, RunSummary } from './tauri';

function baseRun(overrides: Partial<RunSummary> = {}): RunSummary {
  return {
    id: 'r1',
    session_id: 'sess-1',
    work_item_id: 'w1',
    status: 'running',
    pause_reason_class: null,
    task: 'add dark mode',
    stage_index: 0,
    attempt: 0,
    attempts_remaining: 5,
    tier_used: null,
    backend_used: null,
    per_attempt_tokens: null,
    created_at: '2026-07-23T00:00:00Z',
    updated_at: '2026-07-23T00:05:00Z',
    resumable: false,
    ...overrides,
  };
}

function fold(events: RunEvent[]): Job {
  let jobs = new Map<string, Job>();
  for (const e of events) {
    jobs = applyRunEvent(jobs, 'sess-1', e);
  }
  const job = jobs.get('r1');
  if (!job) throw new Error('expected job r1 to exist after folding events');
  return job;
}

describe('runNeedsYou', () => {
  it('is true only for a paused row with the awaiting_approval reason class', () => {
    expect(runNeedsYou('paused', 'awaiting_approval')).toBe(true);
  });

  it('is false for a paused row with any other reason class, or no class at all', () => {
    expect(runNeedsYou('paused', 'retries_exhausted')).toBe(false);
    expect(runNeedsYou('paused', 'explicit_stop')).toBe(false);
    expect(runNeedsYou('paused', null)).toBe(false);
  });

  it('is false for a non-paused status even with the awaiting_approval class stale on the row', () => {
    expect(runNeedsYou('running', 'awaiting_approval')).toBe(false);
    expect(runNeedsYou('interrupted', 'awaiting_approval')).toBe(false);
  });
});

describe('narrateRunStatus', () => {
  it('maps each pause-reason class to a distinct sentence', () => {
    const awaiting = narrateRunStatus('paused', 'awaiting_approval');
    const retries = narrateRunStatus('paused', 'retries_exhausted');
    const explicit = narrateRunStatus('paused', 'explicit_stop');
    const unclassified = narrateRunStatus('paused', null);
    const all = [awaiting, retries, explicit, unclassified];
    expect(new Set(all).size).toBe(4);
  });

  it('renders an interrupted row distinctly from a terminal done/failed row (Success Criteria)', () => {
    const interrupted = narrateRunStatus('interrupted', null);
    const done = narrateRunStatus('done', null);
    const failed = narrateRunStatus('failed', null);
    expect(interrupted).not.toBe(done);
    expect(interrupted).not.toBe(failed);
    expect(interrupted).toContain('tiếp tục');
  });

  it('distinguishes done (success) from failed', () => {
    expect(narrateRunStatus('done', null)).toContain('thành công');
    expect(narrateRunStatus('failed', null)).toContain('thất bại');
  });

  it('falls back to a generic phrase for an unrecognized status', () => {
    expect(narrateRunStatus('some-future-status', null)).toBe('Không rõ trạng thái');
  });
});

describe('runTaskLabel', () => {
  it('returns the trimmed task text when present', () => {
    expect(runTaskLabel('  add dark mode  ')).toBe('add dark mode');
  });

  it('falls back to a generic phrase for null or blank task text', () => {
    expect(runTaskLabel(null)).toBe('một tác vụ');
    expect(runTaskLabel('   ')).toBe('một tác vụ');
  });
});

describe('runStatusBadge', () => {
  it('maps every known pipeline_runs status to a distinct short label', () => {
    const statuses = ['queued', 'running', 'paused', 'interrupted', 'done', 'failed'];
    const labels = statuses.map(runStatusBadge);
    expect(new Set(labels).size).toBe(statuses.length);
  });

  it('falls back for an unrecognized status', () => {
    expect(runStatusBadge('mystery')).toBe('Không rõ');
  });
});

describe('toRowView', () => {
  it('derives everything from the persisted row when no live job is tracked', () => {
    const run = baseRun({ status: 'paused', pause_reason_class: 'awaiting_approval', resumable: false });
    const view = toRowView(run, undefined);
    expect(view.status).toBe('paused');
    expect(view.needsYou).toBe(true);
    expect(view.lastLine).toBe(narrateRunStatus('paused', 'awaiting_approval'));
    expect(view.taskLabel).toBe('add dark mode');
  });

  it('overlays the live job\'s status and last-event narration when tracked', () => {
    const run = baseRun({ status: 'queued' });
    const job = fold([
      { type: 'RunStarted', data: { run_id: 'r1', work_item_id: 'w1' } },
      { type: 'StageStarted', data: { run_id: 'r1', stage: 'build', tier: 'thinking' } },
    ]);
    const view = toRowView(run, job);
    expect(view.status).toBe('running');
    expect(view.lastLine).toContain('build');
  });

  it('always derives needsYou/resumable from the PERSISTED row, never the live job', () => {
    const run = baseRun({
      status: 'paused',
      pause_reason_class: 'awaiting_approval',
      resumable: false,
    });
    const job = fold([{ type: 'RunStarted', data: { run_id: 'r1', work_item_id: 'w1' } }]);
    const view = toRowView(run, job);
    expect(view.needsYou).toBe(true);
    expect(view.resumable).toBe(false);
  });

  it('maps a complete live job to the done status vocabulary', () => {
    const run = baseRun({ status: 'running' });
    const job = fold([
      { type: 'RunStarted', data: { run_id: 'r1', work_item_id: 'w1' } },
      { type: 'RunComplete', data: { run_id: 'r1', outcome: 'done' } },
    ]);
    const view = toRowView(run, job);
    expect(view.status).toBe('done');
  });
});
