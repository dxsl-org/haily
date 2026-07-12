// Splits a unified diff string (from `workspaceDiff`) into per-file segments for
// `DiffViewer`. The text is UNTRUSTED repo content — this module only slices/classifies
// lines as plain strings, never interprets or executes anything from it. The backend
// already caps the overall diff size before it reaches the wire; the caps here are a
// second, client-side guard so a pathologically large diff still can't hang the page
// (phase-11 risk note).
const MAX_LINES_PER_FILE = 500;
const MAX_FILES = 50;

export interface DiffLine {
  kind: 'add' | 'remove' | 'context' | 'meta';
  text: string;
}

export interface DiffFile {
  path: string;
  lines: DiffLine[];
}

export interface ParsedDiff {
  files: DiffFile[];
  filesTruncated: boolean;
}

function classify(line: string): DiffLine['kind'] {
  if (line.startsWith('+++') || line.startsWith('---') || line.startsWith('@@') || line.startsWith('index ')) {
    return 'meta';
  }
  if (line.startsWith('+')) return 'add';
  if (line.startsWith('-')) return 'remove';
  return 'context';
}

/** "diff --git a/foo/bar.rs b/foo/bar.rs" → "foo/bar.rs" (falls back to the raw header
 * if it doesn't match the expected git format — still renders, just less tidy). */
function pathFromHeader(header: string): string {
  const match = header.match(/^diff --git a\/(.+) b\/(.+)$/);
  return match ? match[2] : header.replace(/^diff --git /, '');
}

function capLines(lines: string[]): DiffLine[] {
  const capped = lines.slice(0, MAX_LINES_PER_FILE);
  const out: DiffLine[] = capped.map((text) => ({ kind: classify(text), text }));
  if (lines.length > MAX_LINES_PER_FILE) {
    out.push({ kind: 'meta', text: `… ${lines.length - MAX_LINES_PER_FILE} more lines (truncated)` });
  }
  return out;
}

/**
 * Parse a unified diff into per-file segments, capping both file count and lines-per-file.
 * `filesTruncated` means more files exist than are returned — callers should surface that
 * rather than silently drop it. A blob with no recognizable `diff --git` headers is
 * rendered as a single unnamed file rather than dropped.
 */
export function parseUnifiedDiff(raw: string): ParsedDiff {
  if (!raw.trim()) return { files: [], filesTruncated: false };

  const rawLines = raw.split('\n');
  const headerIdxs: number[] = [];
  rawLines.forEach((line, i) => {
    if (line.startsWith('diff --git ')) headerIdxs.push(i);
  });

  if (headerIdxs.length === 0) {
    return { files: [{ path: '(diff)', lines: capLines(rawLines) }], filesTruncated: false };
  }

  const files: DiffFile[] = [];
  for (let i = 0; i < headerIdxs.length && files.length < MAX_FILES; i++) {
    const start = headerIdxs[i];
    const end = i + 1 < headerIdxs.length ? headerIdxs[i + 1] : rawLines.length;
    const segment = rawLines.slice(start, end);
    files.push({ path: pathFromHeader(segment[0] ?? ''), lines: capLines(segment.slice(1)) });
  }

  return { files, filesTruncated: headerIdxs.length > MAX_FILES };
}
