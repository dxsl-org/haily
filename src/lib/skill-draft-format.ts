// Pure helpers for the structured skill editor (Unified Chat UI phase 9, D4). Kept out of the
// components so they're unit-testable without a DOM — mirrors the `data-view.ts`/`run-events.ts`
// split (formatter/parser logic lives here, components stay thin).
//
// `parseDraftMarkdown` is a FRONTEND-ONLY convenience parse for "draft-with-Haily": it fills the
// editor's 4 fields from the model's plain-text turn output so the user has something to review,
// NOT a security parser. The real section-injection defense lives server-side in
// `haily_kms::skill_editor::markdown` (escape/reject on `render_markdown`/`parse_markdown`) —
// whatever this function produces only ever reaches the backend as a structured `SkillDraft`
// object via `editSkill`, never as raw markdown, so a forged header line here cannot hijack a
// save (see phase-09's Security Considerations).
import type { SkillDraft } from './tauri';

const SECTION_PROCEDURE = '## Procedure';
const SECTION_SUCCESS = '## Success conditions';
const SECTION_FORBIDDEN = '## Forbidden actions';
const SECTION_REQUIRED = '## Required from user';

// Order matters: index in this array is the section index `parseDraftMarkdown` folds lines into.
const CANONICAL_HEADERS: readonly string[] = [SECTION_PROCEDURE, SECTION_SUCCESS, SECTION_FORBIDDEN, SECTION_REQUIRED];

/** Vietnamese labels for the 4 structured fields — single source of truth shared by the editor
 * form (field labels) and `mapSkillSaveError` (attributing a backend error to a field). */
const FIELD_LABELS: Record<keyof SkillDraft, string> = {
  procedure: 'Làm gì (từng bước)',
  success_conditions: 'Điều kiện đúng sau khi xong',
  forbidden_actions: 'Tuyệt đối không',
  required_from_user: 'Cần gì từ tôi trước',
};

export function skillFieldLabel(field: keyof SkillDraft): string {
  return FIELD_LABELS[field];
}

/** Inverse of the server's `escape_field` — strips a defensive leading backslash from a line
 * that would otherwise exactly equal a canonical header, so a field round-trips unchanged. */
function unescapeField(sectionBody: string): string {
  return sectionBody
    .split('\n')
    .map((line) => {
      if (line.startsWith('\\') && CANONICAL_HEADERS.includes(line.slice(1))) {
        return line.slice(1);
      }
      return line;
    })
    .join('\n');
}

/**
 * Parse plain text into the 4-field draft shape. Mirrors
 * `haily_kms::skill_editor::markdown::parse_markdown` line-for-line: only an EXACT, unescaped
 * canonical header line starts a new section; any other content (including a reply that ignored
 * the requested format entirely) folds into `procedure` so nothing from the model's answer is
 * silently dropped — the user always has something to review before Save.
 */
export function parseDraftMarkdown(body: string): SkillDraft {
  const sections: [string, string, string, string] = ['', '', '', ''];
  let current = 0;

  for (const line of body.split('\n')) {
    const idx = CANONICAL_HEADERS.indexOf(line);
    if (idx !== -1) {
      current = idx;
      continue;
    }
    sections[current] = sections[current].length > 0 ? `${sections[current]}\n${line}` : line;
  }

  return {
    procedure: unescapeField(sections[0].trim()),
    success_conditions: unescapeField(sections[1].trim()),
    forbidden_actions: unescapeField(sections[2].trim()),
    required_from_user: unescapeField(sections[3].trim()),
  };
}

/** Strip one wrapping ``` code fence (with an optional language tag) if the whole reply is
 * fenced — a common model habit for anything that looks like a document. A no-op on anything
 * else (including a partial/unbalanced fence), so it never eats real content. */
export function stripCodeFence(text: string): string {
  const trimmed = text.trim();
  const match = trimmed.match(/^```[a-zA-Z]*\n([\s\S]*?)\n```$/);
  return match ? match[1] : text;
}

/**
 * Build the message sent through the normal chat/turn path to produce a draft (D4:
 * draft-with-Haily reuses the ordinary turn path, no dedicated backend command). The 4 section
 * headers are requested verbatim in English because `parseDraftMarkdown` keys on them exactly —
 * everything else is Vietnamese instruction copy, consistent with the rest of the app.
 */
export function buildDraftPrompt(description: string): string {
  return [
    'Hãy soạn nội dung cho một kỹ năng (skill) dựa trên mô tả sau đây.',
    'Trả lời DUY NHẤT theo đúng định dạng bên dưới, giữ nguyên các tiêu đề (không dùng công cụ nào, không thêm lời giải thích ngoài 4 mục này):',
    '',
    SECTION_PROCEDURE,
    '<các bước thực hiện, từng bước một>',
    '',
    SECTION_SUCCESS,
    '<điều kiện coi là đã hoàn thành đúng>',
    '',
    SECTION_FORBIDDEN,
    '<những điều tuyệt đối không được làm>',
    '',
    SECTION_REQUIRED,
    '<những gì cần người dùng cung cấp trước khi bắt đầu>',
    '',
    `Mô tả kỹ năng: ${description}`,
  ].join('\n');
}

/**
 * Map a thrown `edit_skill` error string to the field it concerns, so the caller can surface it
 * next to the offending textarea instead of only in a generic banner. Backend errors are
 * `anyhow` `Display` text (see `haily_kms::skill_editor::ops::validate_draft`/`guard.rs`) —
 * matched by substring since there is no structured error code on the wire. Any message that
 * doesn't match a known shape falls back to a generic (still Vietnamese) banner.
 */
export function mapSkillSaveError(raw: string): { field: keyof SkillDraft | null; message: string } {
  const fieldKeys: (keyof SkillDraft)[] = ['procedure', 'success_conditions', 'forbidden_actions', 'required_from_user'];
  for (const key of fieldKeys) {
    if (raw.includes(`field '${key}'`) && raw.toLowerCase().includes('byte cap')) {
      return { field: key, message: `Nội dung ở mục "${skillFieldLabel(key)}" quá dài — vui lòng rút ngắn lại.` };
    }
  }
  if (
    raw.toLowerCase().includes('traversal') ||
    raw.toLowerCase().includes('may only contain letters') ||
    raw.toLowerCase().includes('reserved device name') ||
    raw.toLowerCase().includes('skill name')
  ) {
    return { field: null, message: 'Tên kỹ năng không hợp lệ để lưu — không thể lưu.' };
  }
  return { field: null, message: `Không thể lưu: ${raw}` };
}
