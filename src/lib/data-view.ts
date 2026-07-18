// Pure formatting/normalization logic for the `DataView` workspace pane (View Engine Phase
// A). Kept out of the components so it's unit-testable without a DOM — mirrors the
// `run-events.ts` split (reducer/formatter logic lives here, components stay thin).
//
// SECURITY (SEC F1 — attribute sinks): `DataView.schema`/`records` may hold MODEL-AUTHORED
// strings (`LlmProjected` provenance). Every formatter below returns a plain string meant for
// a `{expression}` text binding, NEVER for `href=`/`src=`/`style=`/`on*=`. `safeHref` is the
// ONLY function in this module (or anywhere under `src/lib/components/view/`) that may feed
// an `href=` binding — everything else must render as inert text.
import type { EnumVariant, FieldType, ProjectionKind } from './tauri';

/** The two projection kinds this phase's renderer actually implements. */
export type RenderableProjectionKind = 'Table' | 'Cards';

/**
 * Map a `ProjectionKind` to the renderer this phase supports. `Kanban`/`Calendar`/`Chart` (and
 * any future/unrecognized kind) fall back to `Table` — the wire-compat contract documented on
 * `haily_types::ProjectionKind`: a renderer that doesn't implement a kind must still render
 * something, never fail.
 */
export function normalizeProjectionKind(kind: ProjectionKind): RenderableProjectionKind {
  return kind === 'Cards' ? 'Cards' : 'Table';
}

const PROJECTION_LABELS: Record<ProjectionKind, string> = {
  Table: 'Bảng',
  Cards: 'Thẻ',
  Kanban: 'Kanban',
  Calendar: 'Lịch',
  Chart: 'Biểu đồ',
};

/** Display label for a projection-switcher button. Falls back to the raw kind string for a
 * future kind this build doesn't have a label for. */
export function projectionLabel(kind: ProjectionKind): string {
  return PROJECTION_LABELS[kind] ?? kind;
}

const ALLOWED_HREF_SCHEMES = new Set(['http:', 'https:', 'mailto:', 'tel:']);

/**
 * Build a safe `href` for a `Url`/`Email`/`Phone` field value, or `null` when the resulting
 * URL's scheme is not in the allowlist (`http:`/`https:`/`mailto:`/`tel:`) — a `null` result
 * means the caller MUST render the value as inert text instead of a clickable link. This is
 * the sole approved path from a model-authored field value to an `href=` binding anywhere
 * under `src/lib/components/view/` (SEC F1). `kind` picks the URI scheme to prepend for a bare
 * email/phone value; a value that already carries its own scheme (e.g. `mailto:`) is not
 * double-prefixed. Malformed input (unparseable as a URL) also returns `null`.
 */
export function safeHref(value: string, kind: 'url' | 'email' | 'phone'): string | null {
  const trimmed = value.trim();
  if (!trimmed) return null;

  let candidate = trimmed;
  if (kind === 'email' && !/^mailto:/i.test(trimmed)) {
    candidate = `mailto:${trimmed}`;
  } else if (kind === 'phone' && !/^tel:/i.test(trimmed)) {
    candidate = `tel:${trimmed}`;
  }

  let parsed: URL;
  try {
    parsed = new URL(candidate);
  } catch {
    return null;
  }

  return ALLOWED_HREF_SCHEMES.has(parsed.protocol) ? parsed.toString() : null;
}

/** `Money` formatting wrapped in try/catch: a model-authored `currency` (e.g. a malformed or
 * unrecognized ISO 4217 code) makes `Intl.NumberFormat` throw a `RangeError` — fall back to a
 * plain number so one bad field can't break the whole row. */
export function formatMoney(value: unknown, currency: string): string {
  if (value == null) return '';
  const num = typeof value === 'number' ? value : Number(value);
  if (Number.isNaN(num)) return String(value);
  try {
    return new Intl.NumberFormat(undefined, { style: 'currency', currency }).format(num);
  } catch {
    return String(num);
  }
}

/** `Date`/`DateTime` formatting. Falls back to the raw stringified value when it doesn't
 * parse as a date — never throws, never silently renders "Invalid Date". */
export function formatDate(value: unknown, withTime: boolean): string {
  if (typeof value !== 'string' && typeof value !== 'number') {
    return value == null ? '' : String(value);
  }
  const d = new Date(value);
  if (Number.isNaN(d.getTime())) return String(value);
  return withTime ? d.toLocaleString() : d.toLocaleDateString();
}

/** `Enum` label lookup — falls back to the raw stored value when it matches none of the
 * field's declared `variants` (e.g. a value added upstream after this view was projected). */
export function formatEnum(value: unknown, variants: EnumVariant[]): string {
  const raw = value == null ? '' : String(value);
  return variants.find((v) => v.value === raw)?.label ?? raw;
}

/** `Tags` formatting — joins an array value; falls back to a plain stringified scalar. */
export function formatTags(value: unknown): string {
  if (Array.isArray(value)) return value.map((v) => String(v)).join(', ');
  return value == null ? '' : String(value);
}

/**
 * `Reference` formatting — plain label text, no drill-in (out of scope for Phase A). Accepts
 * the common many2one `[id, label]` tuple shape, an `{label}`-shaped object, or a bare scalar.
 */
export function formatReference(value: unknown): string {
  if (Array.isArray(value) && value.length >= 2) return String(value[1]);
  if (value && typeof value === 'object' && 'label' in (value as Record<string, unknown>)) {
    return String((value as Record<string, unknown>).label);
  }
  return value == null ? '' : String(value);
}

/**
 * Format one record's field value per its `FieldType` — the single dispatch point every
 * cell renderer should call. `Opaque` and any unrecognized/future `FieldType` variant (forward
 * compat with a build newer than this frontend) fall back to a plain stringified value rather
 * than throwing, per the "Opaque/unknown → inert text" contract. This function NEVER returns
 * markup — the result is always a plain string for a `{expression}` text binding.
 */
export function formatCellValue(value: unknown, ftype: FieldType): string {
  switch (ftype.type) {
    case 'Money':
      return formatMoney(value, ftype.data.currency);
    case 'Date':
      return formatDate(value, false);
    case 'DateTime':
      return formatDate(value, true);
    case 'Enum':
      return formatEnum(value, ftype.data.variants);
    case 'Reference':
      return formatReference(value);
    case 'Tags':
      return formatTags(value);
    case 'Bool':
      if (value === true) return 'Có';
      if (value === false) return 'Không';
      return '';
    case 'Text':
    case 'LongText':
    case 'Int':
    case 'Float':
    case 'Email':
    case 'Phone':
    case 'Url':
    case 'Opaque':
      return value == null ? '' : String(value);
    default:
      // Forward-compat fallback: a future `FieldType` variant this build doesn't know about
      // still renders as inert text instead of throwing.
      return value == null ? '' : String(value);
  }
}
