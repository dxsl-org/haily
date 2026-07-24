// Pure text-splicing helpers for the slash palette's insert-not-send behavior (P03) —
// kept out of `ChatInput.svelte` so the logic is unit-testable without mounting Svelte,
// and to keep the component under the 200-line file-size convention.

/**
 * The typed filter token when the input is a bare, still-being-typed slash command —
 * e.g. `"/pl"` → `"pl"`. Returns `null` once a space appears (the user has moved on to
 * the argument, per the "only trigger at token start" edge case) or the input doesn't
 * start with `/` at all. Trigger is anchored to the very start of the message, not any
 * `/` mid-text.
 */
export function slashToken(input: string): string | null {
  if (!input.startsWith('/')) return null;
  const rest = input.slice(1);
  return rest.includes(' ') ? null : rest;
}

/**
 * Splice `/<name> ` into `text` over `[start, end)`, returning the new text and the
 * caret position right after the inserted trailing space (ready for the user to type the
 * argument). Never sends — callers are responsible for keeping this insert-only.
 */
export function spliceCommand(
  text: string,
  name: string,
  start: number,
  end: number,
): { text: string; caret: number } {
  const token = `/${name} `;
  return {
    text: text.slice(0, start) + token + text.slice(end),
    caret: start + token.length,
  };
}
