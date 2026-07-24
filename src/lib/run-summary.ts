// Pure, framework-free plain-language mapping for the Runs screen (Unified Chat UI phase 7,
// D6) — mirrors `WorkspaceStatus.ts`'s role for the Workspaces screen. Reuses `run-narration.ts`
// (P04, "the single place that phrase is authored") for a LIVE run's per-event last-line; this
// module supplies the fallback narration for a PERSISTED row with no live overlay, derived
// purely from `pipeline_runs` fields (status + the stored pause-reason class) per the phase's
// Key Insight: status/last-line/needs-you must never depend on possibly-truncated `run_events`.
import { narrate } from './run-narration';
import type { Job } from './run-events';
import type { RunSummary } from './tauri';

/** Raw `pipeline_runs.status` widened to accept any string (the wire type is a plain string,
 * not a TS literal union) — mirrors `WorkspaceStatus.ts`'s `RunStatusRaw`. */
export type RunStatusRaw =
  | 'queued'
  | 'running'
  | 'paused'
  | 'interrupted'
  | 'done'
  | 'failed'
  | (string & Record<never, never>);

const SHORT_STATUS_LABEL: Record<string, string> = {
  queued: 'Chờ',
  running: 'Đang chạy',
  paused: 'Tạm dừng',
  interrupted: 'Gián đoạn',
  done: 'Hoàn tất',
  failed: 'Thất bại',
};

/** Short status-badge word — mirrors `RunProgressCard`'s own `STATUS_LABEL` vocabulary. */
export function runStatusBadge(status: string): string {
  return SHORT_STATUS_LABEL[status] ?? 'Không rõ';
}

/** Plain-language last-action sentence for a PERSISTED row with no live overlay. A synthesized
 * `interrupted` row renders distinctly (its own sentence + resume affordance), never folded
 * into the `failed`/`done` phrasing (phase Success Criteria). */
export function narrateRunStatus(status: string, pauseReasonClass: string | null): string {
  if (status === 'paused') {
    switch (pauseReasonClass) {
      case 'awaiting_approval':
        return 'Đang chờ bạn phê duyệt';
      case 'retries_exhausted':
        return 'Đã tạm dừng — hết lượt thử lại, có thể tiếp tục';
      case 'explicit_stop':
        return 'Đã tạm dừng theo yêu cầu — có thể tiếp tục';
      default:
        return 'Đã tạm dừng';
    }
  }
  switch (status) {
    case 'queued':
      return 'Đang chờ khởi chạy';
    case 'running':
      return 'Đang chạy';
    case 'interrupted':
      return 'Đã gián đoạn — có thể tiếp tục';
    case 'done':
      return 'Đã hoàn tất — thành công';
    case 'failed':
      return 'Đã hoàn tất — thất bại';
    default:
      return 'Không rõ trạng thái';
  }
}

/** The row's task label — falls back to a generic phrase, mirrors `workspaceTaskLabel`. */
export function runTaskLabel(task: string | null): string {
  const trimmed = task?.trim();
  return trimmed && trimmed.length > 0 ? trimmed : 'một tác vụ';
}

/** "Cần bạn" (needs-you) flag — derived ONLY from the stored pause-reason class (Key Insight:
 * never string-match a persisted `RunEvent` reason, which predates the P06 taxonomy). */
export function runNeedsYou(status: string, pauseReasonClass: string | null): boolean {
  return status === 'paused' && pauseReasonClass === 'awaiting_approval';
}

/** One row's fully-derived display state — `RunRow` renders this, never `RunSummary`/`Job`
 * fields directly, so every consumer reads a single consistent shape regardless of source. */
export interface RunRowView {
  id: string;
  sessionId: string;
  taskLabel: string;
  status: RunStatusRaw;
  statusBadge: string;
  lastLine: string;
  needsYou: boolean;
  resumable: boolean;
  createdAt: string;
  updatedAt: string;
}

/** `Job.status` ('running'|'paused'|'complete'|'failed') doesn't share `pipeline_runs.status`'s
 * vocabulary ('queued'|…|'done'|'failed') — mapped here so `RunRow`'s status lookups key off
 * ONE vocabulary regardless of source. */
function mapJobStatus(jobStatus: Job['status']): RunStatusRaw {
  return jobStatus === 'complete' ? 'done' : jobStatus;
}

/** Merge one persisted `RunSummary` with its live reducer job (if this GUI window has one),
 * keyed by run id (`RunSummary.id` === `Job.runId`, the `pipeline_runs` primary key) — live
 * data wins for status/last-line while a run is actively streaming; the persisted row is
 * authoritative the moment no live job is tracked (a restart, or a run this window never
 * observed live). `needsYou`/`resumable` always derive from the persisted row's stored fields
 * (never from the live event log), per the phase's Key Insight. */
export function toRowView(run: RunSummary, liveJob: Job | undefined): RunRowView {
  const lastEvent = liveJob?.events[liveJob.events.length - 1];
  const status = liveJob ? mapJobStatus(liveJob.status) : (run.status as RunStatusRaw);
  const lastLine = lastEvent ? narrate(lastEvent) : narrateRunStatus(run.status, run.pause_reason_class);
  return {
    id: run.id,
    sessionId: run.session_id,
    taskLabel: runTaskLabel(run.task),
    status,
    statusBadge: runStatusBadge(status),
    lastLine,
    needsYou: runNeedsYou(run.status, run.pause_reason_class),
    resumable: run.resumable,
    createdAt: run.created_at,
    updatedAt: run.updated_at,
  };
}
