import { describe, expect, it } from 'vitest';
import {
  formatCellValue,
  formatDate,
  formatEnum,
  formatMoney,
  formatReference,
  formatTags,
  normalizeProjectionKind,
  projectionLabel,
  safeHref,
} from './data-view';
import type { FieldType, ProjectionKind } from './tauri';

describe('normalizeProjectionKind', () => {
  it('passes through the two implemented kinds', () => {
    expect(normalizeProjectionKind('Table')).toBe('Table');
    expect(normalizeProjectionKind('Cards')).toBe('Cards');
  });

  it('falls back to Table for every unimplemented/unknown kind', () => {
    const unimplemented: ProjectionKind[] = ['Kanban', 'Calendar', 'Chart'];
    for (const kind of unimplemented) {
      expect(normalizeProjectionKind(kind)).toBe('Table');
    }
    // A future wire value this build has never heard of must still normalize safely.
    expect(normalizeProjectionKind('SomethingNew' as ProjectionKind)).toBe('Table');
  });
});

describe('projectionLabel', () => {
  it('returns a known label for each declared kind', () => {
    expect(projectionLabel('Table')).toBeTruthy();
    expect(projectionLabel('Cards')).toBeTruthy();
  });

  it('falls back to the raw kind string for an unrecognized value', () => {
    expect(projectionLabel('Unknown' as ProjectionKind)).toBe('Unknown');
  });
});

describe('safeHref', () => {
  it('allows http/https/mailto/tel schemes', () => {
    expect(safeHref('https://example.com/path', 'url')).toBe('https://example.com/path');
    expect(safeHref('http://example.com', 'url')).toBe('http://example.com/');
    expect(safeHref('a@b.com', 'email')).toBe('mailto:a@b.com');
    expect(safeHref('mailto:a@b.com', 'email')).toBe('mailto:a@b.com');
    expect(safeHref('+84901234567', 'phone')).toBe('tel:+84901234567');
    expect(safeHref('tel:+84901234567', 'phone')).toBe('tel:+84901234567');
  });

  it('rejects javascript: and data: schemes for a Url field', () => {
    expect(safeHref('javascript:alert(1)', 'url')).toBeNull();
    expect(safeHref('data:text/html,<script>alert(1)</script>', 'url')).toBeNull();
  });

  it('prefixing an email/phone value never smuggles an executable scheme onto the resulting href', () => {
    // A malicious "email"/"phone" value is still wrapped inside a `mailto:`/`tel:` scheme —
    // the browser treats the whole thing as opaque contact-app input, never as script. The
    // resulting href's OWN scheme (what a webview dispatches on) must still be allowlisted.
    const emailHref = safeHref('javascript:alert(1)', 'email');
    expect(emailHref).not.toBeNull();
    expect(new URL(emailHref as string).protocol).toBe('mailto:');

    const phoneHref = safeHref('javascript:alert(1)', 'phone');
    expect(phoneHref).not.toBeNull();
    expect(new URL(phoneHref as string).protocol).toBe('tel:');
  });

  it('rejects unparseable/empty input', () => {
    expect(safeHref('', 'url')).toBeNull();
    expect(safeHref('   ', 'url')).toBeNull();
    expect(safeHref('not a url', 'url')).toBeNull();
  });
});

describe('formatMoney', () => {
  it('formats a valid currency', () => {
    expect(formatMoney(10, 'USD')).toContain('10');
  });

  it('falls back to a plain number when Intl.NumberFormat throws on a bad currency code', () => {
    // A model-authored currency code that isn't valid ISO 4217 makes `Intl.NumberFormat`
    // throw `RangeError` — this must degrade gracefully, never propagate the throw.
    expect(() => formatMoney(42, 'NOT_A_CURRENCY')).not.toThrow();
    expect(formatMoney(42, 'NOT_A_CURRENCY')).toBe('42');
  });

  it('handles a non-numeric value without throwing', () => {
    expect(formatMoney('abc', 'USD')).toBe('abc');
    expect(formatMoney(null, 'USD')).toBe('');
  });
});

describe('formatDate', () => {
  it('formats a parseable date/datetime', () => {
    expect(formatDate('2026-01-15', false)).not.toBe('Invalid Date');
    expect(formatDate('2026-01-15T10:30:00Z', true)).not.toBe('Invalid Date');
  });

  it('falls back to the raw value for an unparseable date', () => {
    expect(formatDate('not-a-date', false)).toBe('not-a-date');
  });
});

describe('formatEnum', () => {
  const variants = [{ value: 'active', label: 'Đang hoạt động' }];

  it('looks up the label for a known value', () => {
    expect(formatEnum('active', variants)).toBe('Đang hoạt động');
  });

  it('falls back to the raw value for an unknown enum value', () => {
    expect(formatEnum('archived', variants)).toBe('archived');
  });
});

describe('formatTags', () => {
  it('joins array values', () => {
    expect(formatTags(['a', 'b'])).toBe('a, b');
  });

  it('stringifies a bare scalar', () => {
    expect(formatTags('solo')).toBe('solo');
  });
});

describe('formatReference', () => {
  it('reads the label from a many2one [id, label] tuple', () => {
    expect(formatReference([7, 'Alice'])).toBe('Alice');
  });

  it('reads a {label} object shape', () => {
    expect(formatReference({ label: 'Bob' })).toBe('Bob');
  });

  it('stringifies a bare scalar id', () => {
    expect(formatReference(42)).toBe('42');
  });
});

describe('formatCellValue', () => {
  it('formats Money via currency formatting', () => {
    const ftype: FieldType = { type: 'Money', data: { currency: 'USD' } };
    expect(formatCellValue(10, ftype)).toContain('10');
  });

  it('formats Bool as Có/Không', () => {
    expect(formatCellValue(true, { type: 'Bool' })).toBe('Có');
    expect(formatCellValue(false, { type: 'Bool' })).toBe('Không');
  });

  it('renders Opaque as a plain stringified value', () => {
    expect(formatCellValue('raw-blob', { type: 'Opaque' })).toBe('raw-blob');
  });

  it('renders an unrecognized/future FieldType as inert text instead of throwing', () => {
    const future = { type: 'SomeFutureType' } as unknown as FieldType;
    expect(() => formatCellValue('fallback-text', future)).not.toThrow();
    expect(formatCellValue('fallback-text', future)).toBe('fallback-text');
  });

  it('renders Text/Url/Email/Phone as their plain value (link-ification is the caller\'s job)', () => {
    expect(formatCellValue('hello', { type: 'Text' })).toBe('hello');
    expect(formatCellValue('https://example.com', { type: 'Url' })).toBe('https://example.com');
  });
});
