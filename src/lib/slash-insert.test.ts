import { describe, expect, it } from 'vitest';
import { slashToken, spliceCommand } from './slash-insert';

describe('slashToken', () => {
  it('returns the partial token while still typing a leading slash command', () => {
    expect(slashToken('/pl')).toBe('pl');
    expect(slashToken('/')).toBe('');
  });

  it('returns null once a space appears — the user moved on to the argument', () => {
    expect(slashToken('/plan do the thing')).toBeNull();
    expect(slashToken('/plan ')).toBeNull();
  });

  it('returns null for a "/" that is not at the start of the message', () => {
    expect(slashToken('hello /plan')).toBeNull();
    expect(slashToken('  /plan')).toBeNull();
  });

  it('returns null for text with no leading slash at all', () => {
    expect(slashToken('hello there')).toBeNull();
    expect(slashToken('')).toBeNull();
  });
});

describe('spliceCommand', () => {
  it('replaces the entire typed token when start=0, end=text.length (inline trigger)', () => {
    const result = spliceCommand('/pl', 'plan', 0, 3);
    expect(result.text).toBe('/plan ');
    expect(result.caret).toBe('/plan '.length);
  });

  it('inserts at an arbitrary cursor position without disturbing surrounding text (＋ menu)', () => {
    const result = spliceCommand('hello world', 'undo', 6, 6);
    expect(result.text).toBe('hello /undo world');
    expect(result.caret).toBe('hello /undo '.length);
  });

  it('replaces a non-empty selection range', () => {
    const result = spliceCommand('hello world', 'build', 6, 11);
    expect(result.text).toBe('hello /build ');
  });
});
