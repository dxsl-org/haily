// Pure event-folding logic for the cockpit's `RunTimeline` (P11b). Kept out of the
// component so the reducer is unit-testable and RunJobCard/RunTimeline stay thin.
// `RunEvent` carries no explicit "run finished" boolean besides `RunComplete.outcome`,
// which is a free-form string from the runner (P4) — not a typed enum — so status
// derivation below is a best-effort heuristic, not an authoritative field.
import type { RunEvent } from './tauri';

export type JobStatus = 'running' | 'paused' | 'complete' | 'failed';

/** One coding run as a long-lived job (not a chat bubble) — the ordered `events` log is
 * authoritative; the other fields are just a derived headline for the collapsed card. */
export interface Job {
  runId: string;
  sessionId: string;
  workItemId: string;
  status: JobStatus;
  currentStage: string | null;
  currentTier: string | null;
  lastAttempt: number | null;
  events: RunEvent[];
  /** Client wall-clock ms at the first event this GUI observed for the run (NOT the
   * backend's actual start time — this reducer is purely event-sourced, see the module
   * doc). Used by `RunProgressCard`'s elapsed-time display (P04). */
  startedAt: number;
  /** Client wall-clock ms when `RunComplete` landed, or `null` while still running/paused
   * — freezes the elapsed-time display once a run finishes instead of letting it keep
   * counting up past completion (P04). */
  completedAt: number | null;
}

/**
 * Fold one incoming `RunEvent` into the job map, returning a NEW map (immutable update
 * for Svelte reactivity — never mutate `jobs` in place). Every event is appended to
 * `events` in arrival order: the ordered, non-coalescing `haily-run-events` bridge
 * guarantees nothing is dropped or reordered, so the raw log is the source of truth for
 * `RunJobCard`'s expanded view; the other fields are only a convenience summary.
 */
export function applyRunEvent(jobs: Map<string, Job>, sessionId: string, event: RunEvent): Map<string, Job> {
  const runId = event.data.run_id;
  const next = new Map(jobs);
  const existing = next.get(runId);
  const job: Job = existing
    ? { ...existing, events: [...existing.events, event] }
    : {
        runId,
        sessionId,
        workItemId: event.type === 'RunStarted' ? event.data.work_item_id : '',
        status: 'running',
        currentStage: null,
        currentTier: null,
        lastAttempt: null,
        events: [event],
        startedAt: Date.now(),
        completedAt: null,
      };

  if (event.type === 'StageStarted') {
    job.currentStage = event.data.stage;
    job.currentTier = event.data.tier ?? job.currentTier;
    job.status = 'running';
  } else if (event.type === 'RunPaused') {
    job.status = 'paused';
  } else if (event.type === 'Retry') {
    job.lastAttempt = event.data.attempt;
    job.status = 'running';
  } else if (event.type === 'Escalation') {
    job.currentTier = event.data.to;
  } else if (event.type === 'RunComplete') {
    job.status = /fail|error/i.test(event.data.outcome) ? 'failed' : 'complete';
    job.completedAt = Date.now();
  }

  next.set(runId, job);
  return next;
}

/** Newest-first job order for the timeline list — the most recently active run on top. */
export function orderedJobs(jobs: Map<string, Job>): Job[] {
  return [...jobs.values()].reverse();
}

/** Number of verifier-grounded retries recorded so far for a job — derived from the
 * ordered event log rather than a separate counter field (DRY: `events` is already the
 * source of truth). Reused by `RunProgressCard` (P04) and P07's Runs-list row. */
export function retryCount(job: Job): number {
  return job.events.filter((e) => e.type === 'Retry').length;
}

/** Number of model-tier escalations recorded so far for a job — same derivation style as
 * `retryCount`. */
export function escalationCount(job: Job): number {
  return job.events.filter((e) => e.type === 'Escalation').length;
}

/** Formats a millisecond duration as `mm:ss` (or `h:mm:ss` past one hour) for
 * `RunProgressCard`'s elapsed-time label (P04). Negative/invalid input clamps to zero
 * rather than rendering a negative or NaN duration. */
export function formatElapsed(ms: number): string {
  const totalSec = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  const mm = String(m).padStart(2, '0');
  const ss = String(s).padStart(2, '0');
  return h > 0 ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
}

export interface EventDescriptor {
  icon: string;
  text: string;
  tone: 'info' | 'pass' | 'fail' | 'warn';
}

/** Render one `RunEvent` as an inert text line for `RunJobCard`/`RunEventLog`. `text` is
 * built from UNTRUSTED, already tag-stripped repo/tool content (`StageOutput.chunk`,
 * `GateResult.decisive`, `DiffAvailable.file`, `PlanReady.plan_path`) — callers MUST bind
 * this via `{text}`, never `{@html}`.
 *
 * TOTAL, never throws (review fix, phase-04): an unrecognized future variant (frontend
 * build older than the backend that emitted it) degrades to a generic descriptor rather
 * than throwing — a throw here would crash the WHOLE expanded event list around one
 * unmapped row, defeating the same deploy-skew resilience `narrate()` already provides
 * for the collapsed card's last-action line in the exact same view. */
export function describeEvent(e: RunEvent): EventDescriptor {
  switch (e.type) {
    case 'RunStarted':
      return { icon: '▶', text: `Run started — work item ${e.data.work_item_id}`, tone: 'info' };
    case 'StageStarted':
      return {
        icon: '▶',
        text: `Stage: ${e.data.stage}${e.data.tier ? ` (${e.data.tier})` : ''}`,
        tone: 'info',
      };
    case 'StageOutput':
      return { icon: '·', text: e.data.chunk, tone: 'info' };
    case 'GateResult':
      return {
        icon: e.data.pass ? '✓' : '✗',
        text: `Gate ${e.data.gate}: ${e.data.pass ? 'passed' : 'failed'} — ${e.data.decisive}`,
        tone: e.data.pass ? 'pass' : 'fail',
      };
    case 'Retry':
      return { icon: '↻', text: `Retry — attempt ${e.data.attempt}`, tone: 'warn' };
    case 'Escalation':
      return { icon: '⇧', text: `Escalated ${e.data.from} → ${e.data.to}`, tone: 'warn' };
    case 'DiffAvailable':
      return { icon: '✎', text: `Diff available: ${e.data.file}`, tone: 'info' };
    case 'ApprovalNeeded':
      return { icon: '⚠', text: `Approval needed (id …${e.data.approval_id.slice(-8)})`, tone: 'warn' };
    case 'PlanReady':
      return { icon: '\u{1F4CB}', text: `Plan ready: ${e.data.plan_path}`, tone: 'info' };
    case 'RunPaused':
      return { icon: '⏸', text: `Paused — ${e.data.reason}`, tone: 'warn' };
    case 'RunComplete': {
      const failed = /fail|error/i.test(e.data.outcome);
      return { icon: failed ? '✗' : '✓', text: `Run complete — ${e.data.outcome}`, tone: failed ? 'fail' : 'pass' };
    }
    default:
      return { icon: '•', text: 'Sự kiện chưa được hỗ trợ', tone: 'info' };
  }
}
