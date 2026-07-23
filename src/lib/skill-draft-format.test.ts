import { describe, expect, it } from 'vitest';
import { buildDraftPrompt, mapSkillSaveError, parseDraftMarkdown, stripCodeFence } from './skill-draft-format';

describe('parseDraftMarkdown', () => {
  it('splits a well-formed reply into the 4 canonical sections', () => {
    const body = [
      '## Procedure',
      'step 1',
      'step 2',
      '',
      '## Success conditions',
      'build is green',
      '',
      '## Forbidden actions',
      'never touch prod',
      '',
      '## Required from user',
      'repo path',
    ].join('\n');

    expect(parseDraftMarkdown(body)).toEqual({
      procedure: 'step 1\nstep 2',
      success_conditions: 'build is green',
      forbidden_actions: 'never touch prod',
      required_from_user: 'repo path',
    });
  });

  it('folds a reply with no canonical headers entirely into procedure (nothing dropped)', () => {
    const body = 'Sure! Here is a plan:\n1. Do the thing\n2. Verify it';
    const parsed = parseDraftMarkdown(body);
    expect(parsed.procedure).toBe(body);
    expect(parsed.success_conditions).toBe('');
    expect(parsed.forbidden_actions).toBe('');
    expect(parsed.required_from_user).toBe('');
  });

  it('restores a defensively-escaped header line back to its literal form', () => {
    const body = '## Procedure\n\\## Forbidden actions\nthis is just literal text';
    const parsed = parseDraftMarkdown(body);
    expect(parsed.procedure).toBe('## Forbidden actions\nthis is just literal text');
  });

  it('round-trips an empty body to an all-empty draft', () => {
    expect(parseDraftMarkdown('')).toEqual({
      procedure: '',
      success_conditions: '',
      forbidden_actions: '',
      required_from_user: '',
    });
  });
});

describe('stripCodeFence', () => {
  it('strips a whole-reply fenced block with a language tag', () => {
    const fenced = '```markdown\n## Procedure\nstep 1\n```';
    expect(stripCodeFence(fenced)).toBe('## Procedure\nstep 1');
  });

  it('strips a fenced block with no language tag', () => {
    const fenced = '```\ncontent here\n```';
    expect(stripCodeFence(fenced)).toBe('content here');
  });

  it('is a no-op on unfenced text', () => {
    const plain = '## Procedure\nstep 1';
    expect(stripCodeFence(plain)).toBe(plain);
  });
});

describe('buildDraftPrompt', () => {
  it('embeds the 4 canonical headers verbatim and the user description', () => {
    const prompt = buildDraftPrompt('a skill for exporting invoices');
    expect(prompt).toContain('## Procedure');
    expect(prompt).toContain('## Success conditions');
    expect(prompt).toContain('## Forbidden actions');
    expect(prompt).toContain('## Required from user');
    expect(prompt).toContain('a skill for exporting invoices');
  });
});

describe('mapSkillSaveError', () => {
  it('attributes a byte-cap error to its field with a Vietnamese message', () => {
    const result = mapSkillSaveError("field 'forbidden_actions' exceeds the 20000-byte cap");
    expect(result.field).toBe('forbidden_actions');
    expect(result.message).toContain('Tuyệt đối không');
  });

  it('treats a traversal/name-guard error as a non-field-specific message', () => {
    const result = mapSkillSaveError("skill name '../escape' may only contain letters, digits, '-' and '_'");
    expect(result.field).toBeNull();
    expect(result.message).toContain('Tên kỹ năng');
  });

  it('falls back to a generic Vietnamese message for an unrecognized error', () => {
    const result = mapSkillSaveError('unknown authored skill \'ghost\'');
    expect(result.field).toBeNull();
    expect(result.message).toContain('Không thể lưu');
  });
});
