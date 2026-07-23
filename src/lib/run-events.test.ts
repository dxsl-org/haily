import { describe, expect, it } from 'vitest';
import { applyRunEvent, escalationCount, retryCount, type Job } from './run-events';
import type { RunEvent } from './tauri';

function fold(events: RunEvent[]): Job {
  let jobs = new Map<string, Job>();
  for (const e of events) {
    jobs = applyRunEvent(jobs, 'sess-1', e);
  }
  const job = jobs.get('r1');
  if (!job) throw new Error('expected job r1 to exist after folding events');
  return job;
}

describe('applyRunEvent — startedAt/completedAt (P04 extension)', () => {
  it('stamps startedAt once on the first event and never changes it on later folds', () => {
    let jobs = new Map<string, Job>();
    jobs = applyRunEvent(jobs, 'sess-1', { type: 'RunStarted', data: { run_id: 'r1', work_item_id: 'w1' } });
    const first = jobs.get('r1')!.startedAt;
    jobs = applyRunEvent(jobs, 'sess-1', { type: 'StageStarted', data: { run_id: 'r1', stage: 'build' } });
    expect(jobs.get('r1')!.startedAt).toBe(first);
    expect(jobs.get('r1')!.completedAt).toBeNull();
  });

  it('stamps completedAt only once RunComplete lands', () => {
    const job = fold([
      { type: 'RunStarted', data: { run_id: 'r1', work_item_id: 'w1' } },
      { type: 'RunComplete', data: { run_id: 'r1', outcome: 'done' } },
    ]);
    expect(job.completedAt).not.toBeNull();
    expect(job.completedAt!).toBeGreaterThanOrEqual(job.startedAt);
  });
});

describe('retryCount / escalationCount', () => {
  it('counts Retry and Escalation events independently, ignoring other event types', () => {
    const job = fold([
      { type: 'RunStarted', data: { run_id: 'r1', work_item_id: 'w1' } },
      { type: 'StageStarted', data: { run_id: 'r1', stage: 'build' } },
      { type: 'Retry', data: { run_id: 'r1', attempt: 1 } },
      { type: 'Escalation', data: { run_id: 'r1', from: 'fast', to: 'thinking' } },
      { type: 'Retry', data: { run_id: 'r1', attempt: 2 } },
    ]);
    expect(retryCount(job)).toBe(2);
    expect(escalationCount(job)).toBe(1);
  });

  it('returns 0 for a job with neither event', () => {
    const job = fold([{ type: 'RunStarted', data: { run_id: 'r1', work_item_id: 'w1' } }]);
    expect(retryCount(job)).toBe(0);
    expect(escalationCount(job)).toBe(0);
  });
});
