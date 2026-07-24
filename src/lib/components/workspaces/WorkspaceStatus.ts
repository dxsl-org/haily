// Pure, framework-free plain-language status mapping for the Workspaces screen (Unified Chat
// UI phase 10, D6). Deliberately has NO git vocabulary in its output — the whole point of this
// module is to be the one place that decides the user-facing label, so `WorkspaceRow.svelte`
// never has to reach for `run_status`/`dirty`/`worktree_reclaimed` combinations itself. The VN
// vocabulary here is LOCKED by the plan: đang chạy / chờ áp dụng / đã áp dụng / đã dọn dẹp —
// do not introduce a fifth label or rename these without updating the plan.

/** Raw `pipeline_runs.status` as returned by `WorkspaceView.run_status` (`null` = no linked run).
 * Widened to accept any string beyond the known set (rather than casting with `any` at the call
 * site) since the wire type is a plain `string | null`, not a TS literal union. */
export type RunStatusRaw =
  | 'queued'
  | 'running'
  | 'paused'
  | 'interrupted'
  | 'done'
  | 'failed'
  | (string & Record<never, never>)
  | null;

export interface WorkspaceStatusInput {
  runStatus: RunStatusRaw;
  dirty: boolean;
  /** `WorkspaceView.worktree_reclaimed` — the worktree directory itself is gone. */
  reclaimed: boolean;
}

export type WorkspaceStatusLabel = 'đang chạy' | 'chờ áp dụng' | 'đã áp dụng' | 'đã dọn dẹp';

/**
 * Map a workspace's (run status, dirty, reclaimed) triple to its plain-language status.
 * Priority order matters: a live run always reads as running even on a dirty tree; a reclaimed
 * worktree always reads as cleaned-up even if the caller still thinks the run is non-terminal
 * (a defensive ordering — reclaimed should never co-occur with a live run in practice, but if it
 * ever did, "already cleaned up" is the safer thing to tell the user than "running").
 */
export function workspaceStatusLabel(input: WorkspaceStatusInput): WorkspaceStatusLabel {
  const { runStatus, dirty, reclaimed } = input;
  if (reclaimed) return 'đã dọn dẹp';
  if (runStatus === 'queued' || runStatus === 'running') return 'đang chạy';
  if (dirty) return 'chờ áp dụng';
  return 'đã áp dụng';
}

/**
 * Short explanatory hint shown under the status badge — plain language, no git terms.
 * `hasRun` is `false` for a workspace with NO linked run at all (never stamped a `run_id`, and
 * no in-flight run found for its session) — the passive "orphan" case the P4 worktree reaper
 * eventually reclaims on its own TTL. Rather than a fifth status label (the vocabulary is
 * LOCKED to the four above), an orphan surfaces as a nuance of its existing đã áp dụng/chờ áp
 * dụng label PLUS this hint — passive information, never an action (per the plan's reaper note).
 */
export function workspaceStatusHint(label: WorkspaceStatusLabel, hasRun: boolean): string {
  switch (label) {
    case 'đang chạy':
      return 'Haily đang thực hiện tác vụ này.';
    case 'chờ áp dụng':
      return hasRun
        ? 'Có thay đổi đang chờ bạn xem và áp dụng.'
        : 'Có thay đổi nhưng không có lượt chạy nào đang gắn với nó — sẽ tự động được dọn dẹp nếu không dùng đến.';
    case 'đã áp dụng':
      return hasRun
        ? 'Không có thay đổi nào đang chờ.'
        : 'Không có tiến trình nào đang chạy — sẽ tự động được dọn dẹp nếu không dùng đến.';
    case 'đã dọn dẹp':
      return 'Không gian làm việc này đã được dọn dẹp và không thể tiếp tục.';
  }
}

/** The row's task label — falls back to a generic phrase when no run is linked yet. */
export function workspaceTaskLabel(task: string | null): string {
  const trimmed = task?.trim();
  return trimmed && trimmed.length > 0 ? trimmed : 'một tác vụ';
}
