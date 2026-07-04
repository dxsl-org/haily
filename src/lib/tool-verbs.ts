// Human-verb framing for tool approval cards + journal diffs (R4, Phase 3).
// Runs entirely in the frontend — no LLM call, no backend generation (see phase file
// "Architecture": "Verb map lives in the FRONTEND"). Every value here is rendered through
// Svelte's `{expression}` auto-escaping by the caller; this module only returns plain
// strings, never HTML, so a crafted title/content can't become a script (XSS).
//
// `args`/`preState`/`postState` are UNTRUSTED — they round-trip through the LLM and,
// for journal rows, through whatever the connector/local write recorded. Every extractor
// below reads a closed, hand-picked set of JSON keys per tool/table; it never iterates or
// dumps arbitrary object keys, so a poisoned payload can add junk fields without them
// ever reaching the UI.

/** Parse a JSON string defensively — untrusted input may be malformed or not an object. */
function parseObject(json: string | null | undefined): Record<string, unknown> | null {
  if (!json) return null;
  try {
    const value: unknown = JSON.parse(json);
    return value && typeof value === 'object' && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

/** Read a whitelisted string field, or `undefined` if absent/not a string. */
function str(obj: Record<string, unknown> | null, key: string): string | undefined {
  const v = obj?.[key];
  return typeof v === 'string' && v.length > 0 ? v : undefined;
}

/** Truncate a display value so a pathologically long title can't blow out the card layout. */
function clip(value: string, max = 80): string {
  return value.length > max ? `${value.slice(0, max)}…` : value;
}

/**
 * One verb template per known v1 tool name (see `crates/haily-tools/src/lib.rs::build_v1`
 * for the authoritative list). `args` is the raw JSON string from the approval chunk;
 * each template pulls only the specific keys that tool's `execute()` reads.
 */
const VERB_TEMPLATES: Record<string, (args: Record<string, unknown> | null) => string> = {
  task_create: (a) => `Tạo task "${clip(str(a, 'title') ?? '(không tên)')}"?`,
  task_complete: (a) => `Đánh dấu hoàn thành task${idSuffix(a)}?`,
  task_delete: (a) => `Xóa task${idSuffix(a)}?`,
  note_save: (a) => `Lưu ghi chú "${clip(str(a, 'title') ?? '(không tên)')}"?`,
  note_update: (a) => `Cập nhật ghi chú "${clip(str(a, 'title') ?? '(không tên)')}"?`,
  note_delete: (a) => `Xóa ghi chú${idSuffix(a)}?`,
  reminder_add: (a) => `Đặt nhắc nhở "${clip(str(a, 'title') ?? '(không tên)')}"?`,
  reminder_delete: (a) => `Xóa nhắc nhở${idSuffix(a)}?`,
  calendar_add: (a) => `Tạo sự kiện "${clip(str(a, 'title') ?? '(không tên)')}"?`,
  calendar_delete: (a) => `Xóa sự kiện lịch${idSuffix(a)}?`,
  memory_remember: (a) => {
    const subject = str(a, 'subject');
    const predicate = str(a, 'predicate');
    const object = str(a, 'object');
    if (subject && predicate && object) {
      return `Ghi nhớ: "${clip(subject)} ${clip(predicate)} ${clip(object)}"?`;
    }
    return 'Ghi nhớ thông tin mới?';
  },
  memory_forget: (a) => `Quên vĩnh viễn một thông tin đã lưu${idSuffix(a)}?`,
  work_item_resume: (a) => `Tiếp tục công việc dang dở${idSuffix(a)}?`,
  feedback_react: (a) => {
    const reaction = str(a, 'reaction');
    if (reaction === 'correction') return 'Sửa lại một thông tin đã ghi nhớ?';
    if (reaction === 'negative') return 'Ghi nhận phản hồi không hài lòng?';
    return 'Ghi nhận phản hồi?';
  },
  worktree_apply: (a) => {
    const confirm = a?.['confirm'];
    return confirm === true
      ? 'Áp dụng thay đổi từ sandbox vào workspace chính?'
      : 'Xem thay đổi từ sandbox?';
  },
};

function idSuffix(args: Record<string, unknown> | null): string {
  const id = str(args, 'id');
  return id ? ` (id: ${clip(id, 24)})` : '';
}

/**
 * Human-readable verb phrase for an approval card. `name` is the tool name from the
 * `ToolApprovalRequest`/`ToolResult` chunk; `args` is that chunk's raw JSON `args` string.
 * Falls back to a generic phrase for any tool not in the whitelist above (new/renamed
 * tools degrade to this rather than the caller guessing at unknown shapes).
 */
export function toolVerb(name: string, args: string): string {
  const template = VERB_TEMPLATES[name];
  if (!template) return `Thực hiện thao tác: ${name}?`;
  return template(parseObject(args));
}

/** One human-readable "field changed from X to Y" pair for the journal diff card. */
export interface DiffField {
  label: string;
  before: string;
  after: string;
}

/** Per-table whitelist of (column key → display label) — mirrors the columns
 * `LocalTable::whitelisted_columns` snapshots in `crates/haily-db/src/queries/local_snapshot.rs`.
 * Deliberately a subset: only fields worth surfacing as a human diff (ids/timestamps/internal
 * foreign keys are omitted even though the backend snapshots them). */
const DIFF_FIELDS: Record<string, Array<{ key: string; label: string }>> = {
  tasks: [
    { key: 'title', label: 'Tiêu đề' },
    { key: 'description', label: 'Mô tả' },
    { key: 'priority', label: 'Độ ưu tiên' },
    { key: 'status', label: 'Trạng thái' },
    { key: 'due_at', label: 'Hạn' },
  ],
  notes: [
    { key: 'title', label: 'Tiêu đề' },
    { key: 'content', label: 'Nội dung' },
    { key: 'tags', label: 'Thẻ' },
  ],
  reminders: [
    { key: 'title', label: 'Tiêu đề' },
    { key: 'fire_at', label: 'Thời gian' },
    { key: 'recurrence', label: 'Lặp lại' },
  ],
};

/** Guess which whitelist to use from the tool name — `journal.toolName` is the tool that
 * produced the row (e.g. `task_delete`), not the table, so this maps the tool's prefix to
 * its table's diff-field set. Unknown prefixes fall back to no fields (empty diff). */
function tableForTool(toolName: string): keyof typeof DIFF_FIELDS | null {
  if (toolName.startsWith('task_')) return 'tasks';
  if (toolName.startsWith('note_')) return 'notes';
  if (toolName.startsWith('reminder_')) return 'reminders';
  return null;
}

/**
 * Extract whitelisted before/after field pairs from a journal row's `preState`/`postState`
 * (see `JournalEntry` in `tauri.ts`). Only fields that actually differ are returned, and only
 * for whitelisted keys — never a raw dump of either JSON blob. Either state may be `null`
 * (e.g. `preState` is null for a create); a missing side renders as `(không có)`.
 */
export function extractDiff(
  toolName: string,
  preState: string | null,
  postState: string | null,
): DiffField[] {
  const table = tableForTool(toolName);
  if (!table) return [];

  const pre = parseObject(preState);
  const post = parseObject(postState);
  if (!pre && !post) return [];

  const fields: DiffField[] = [];
  for (const { key, label } of DIFF_FIELDS[table]) {
    const before = str(pre, key) ?? '(không có)';
    const after = str(post, key) ?? '(không có)';
    if (before !== after) {
      fields.push({ label, before: clip(before), after: clip(after) });
    }
  }
  return fields;
}
