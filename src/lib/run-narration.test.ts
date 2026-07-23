import { describe, expect, it } from 'vitest';
import { narrate } from './run-narration';
import type { RunEvent } from './tauri';

// One representative sample per declared `RunEvent` variant (`crates/haily-types/src/lib.rs`
// RunEvent enum) — asserts the exhaustive-coverage guarantee documented on `narrate`.
const SAMPLES: RunEvent[] = [
  { type: 'RunStarted', data: { run_id: 'r1', work_item_id: 'w1' } },
  { type: 'StageStarted', data: { run_id: 'r1', stage: 'build', tier: 'thinking' } },
  { type: 'StageStarted', data: { run_id: 'r1', stage: 'build' } },
  { type: 'StageOutput', data: { run_id: 'r1', seq: 1, chunk: 'raw log text' } },
  { type: 'GateResult', data: { run_id: 'r1', gate: 'command', pass: true, decisive: '' } },
  { type: 'GateResult', data: { run_id: 'r1', gate: 'command', pass: false, decisive: 'boom' } },
  { type: 'Retry', data: { run_id: 'r1', attempt: 2 } },
  { type: 'Escalation', data: { run_id: 'r1', from: 'fast', to: 'thinking' } },
  { type: 'DiffAvailable', data: { run_id: 'r1', file: 'src/x.rs' } },
  { type: 'ApprovalNeeded', data: { run_id: 'r1', approval_id: 'a1' } },
  { type: 'PlanReady', data: { run_id: 'r1', plan_path: '.agents/x/plan.md' } },
  { type: 'RunPaused', data: { run_id: 'r1', reason: 'retries_exhausted' } },
  { type: 'RunComplete', data: { run_id: 'r1', outcome: 'done' } },
  { type: 'RunComplete', data: { run_id: 'r1', outcome: 'failed: gate' } },
];

describe('narrate', () => {
  it('maps every known RunEvent variant to a non-empty, non-fallback Vietnamese phrase', () => {
    for (const event of SAMPLES) {
      const text = narrate(event);
      expect(text.length).toBeGreaterThan(0);
      expect(text).not.toBe('Đang xử lý…');
    }
  });

  it('never echoes untrusted payload text (StageOutput.chunk) into the narration', () => {
    expect(narrate({ type: 'StageOutput', data: { run_id: 'r1', seq: 1, chunk: 'DROP TABLE x' } })).not.toContain(
      'DROP TABLE',
    );
  });

  it('falls back to a generic verb for an unrecognized future variant', () => {
    const unknown = { type: 'SomethingNewFromTheFuture', data: { run_id: 'r1' } } as unknown as RunEvent;
    expect(narrate(unknown)).toBe('Đang xử lý…');
  });

  it('distinguishes pass/fail phrasing for GateResult and RunComplete', () => {
    expect(narrate({ type: 'GateResult', data: { run_id: 'r1', gate: 'g', pass: true, decisive: '' } })).toContain(
      'Đã qua',
    );
    expect(narrate({ type: 'GateResult', data: { run_id: 'r1', gate: 'g', pass: false, decisive: '' } })).toContain(
      'Không qua',
    );
    expect(narrate({ type: 'RunComplete', data: { run_id: 'r1', outcome: 'ok' } })).toContain('thành công');
    expect(narrate({ type: 'RunComplete', data: { run_id: 'r1', outcome: 'error: crash' } })).toContain('thất bại');
  });
});
