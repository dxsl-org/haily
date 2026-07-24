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
 * Takes the raw `runStatus` (not just a `hasRun` boolean, per review MED/LOW-3 follow-up) so it
 * can distinguish an `interrupted`/`failed` run's clean tree — nothing was ever applied, the run
 * just stopped — from a genuine "no changes pending" clean tree; without this a row could read
 * "đã áp dụng" (applied) while ALSO offering "Tiếp tục", overclaiming success for a stopped run.
 * A `runStatus` of `null` is the passive "orphan" case the P4 worktree reaper eventually reclaims
 * on its own TTL — rather than a fifth status label (the vocabulary is LOCKED to the four above),
 * it surfaces as a nuance of its existing đã áp dụng/chờ áp dụng label PLUS this hint — passive
 * information, never an action (per the plan's reaper note).
 */
export function workspaceStatusHint(label: WorkspaceStatusLabel, runStatus: RunStatusRaw): string {
  const hasRun = runStatus !== null;
  switch (label) {
    case 'đang chạy':
      return 'Haily đang thực hiện tác vụ này.';
    case 'chờ áp dụng':
      return hasRun
        ? 'Có thay đổi đang chờ bạn xem và áp dụng.'
        : 'Có thay đổi nhưng không có lượt chạy nào đang gắn với nó — sẽ tự động được dọn dẹp nếu không dùng đến.';
    case 'đã áp dụng':
      if (runStatus === 'interrupted' || runStatus === 'failed') {
        return 'Tạm dừng giữa chừng, chưa có thay đổi nào được áp dụng — có thể tiếp tục.';
      }
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

/**
 * Static VN copy rendered by `WorkspaceRow.svelte` (review LOW-4 follow-up): pulling every
 * fixed row/button string into one exported table lets the no-git-terms test assert on the
 * ACTUAL rendered copy, not just this module's label/hint output — the surface most likely to
 * leak a git term on a future edit (a stray "branch"/"worktree" in a button label or warning).
 * Dynamic parts (task name, file count, live error text) are excluded — those are user/LLM/
 * backend text already covered by the defensive plain-text-rendering requirement, not fixed copy.
 */
export const ROW_COPY = {
  taskPrefix: 'Bản làm việc riêng cho',
  changedFilesSuffix: 'tệp thay đổi',
  nullSandboxWarning:
    '⚠ Không có lớp cách ly (sandbox) trên máy này — mọi lệnh chạy trực tiếp, cần bạn phê duyệt thủ công cho mỗi hành động lần đầu.',
  showDiff: 'Xem thay đổi',
  hideDiff: 'Ẩn thay đổi',
  discard: 'Huỷ',
  discarding: 'Đang huỷ…',
  resume: 'Tiếp tục',
  resuming: 'Đang tiếp tục…',
  /** Review MED follow-up: `QueuedApproval` carries no tool name (the broker never learns the
   * descriptive payload — see `haily-core::approval::PendingApproval`'s own doc), so a matched
   * approval on this workspace's session might be an UNRELATED gate (e.g. a NullSandbox
   * first-exec prompt), not necessarily the worktree_apply request. This notice never claims to
   * identify which — it points the user to Chat, where the full `ToolApprovalRequest` payload
   * (tool name + args) is actually shown, instead of offering a specific-sounding "Áp dụng"/
   * "Từ chối" pair the session-only correlation cannot honestly back. */
  pendingApprovalNotice: 'Có yêu cầu phê duyệt đang chờ trên phiên này — mở Chat để xem chi tiết.',
} as const;
