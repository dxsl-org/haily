// Reactive palette-open/filter state shared by the "/" inline trigger and the ＋ button
// (P03, D6) — both entry paths must converge on the exact same open/filter state so
// behavior can't diverge (D6: "two paths, one destination"). Extracted out of
// `ChatInput.svelte` to keep that component under the 200-line file-size convention;
// Svelte 5 runes work identically in a `.svelte.ts` module as in a component.
import { slashToken } from './slash-insert';

/**
 * `getInput` must return the live input text (a closure over the caller's `$state`, e.g.
 * `() => input`) — reading it inside the `$derived` below registers the dependency
 * across the module boundary the same way it would inline in a component.
 */
export function createSlashPaletteState(getInput: () => string) {
  let plusMenuOpen = $state(false);
  // Esc/select escape hatch; cleared on the next keystroke (`onTyped`) so retyping can
  // reopen the palette rather than staying dismissed for the rest of the session.
  let dismissed = $state(false);

  const inlineFilter = $derived(slashToken(getInput()));
  const open = $derived((inlineFilter !== null || plusMenuOpen) && !dismissed);
  const filter = $derived(plusMenuOpen ? '' : (inlineFilter ?? ''));

  function close() {
    plusMenuOpen = false;
    dismissed = true;
  }

  function togglePlus() {
    if (plusMenuOpen) {
      close();
    } else {
      dismissed = false;
      plusMenuOpen = true;
    }
  }

  return {
    get open() {
      return open;
    },
    get filter() {
      return filter;
    },
    /** Whether the palette is open via the ＋ button (vs. the inline "/" trigger) —
     * `insertCommand` needs this to decide whether to replace the whole typed token or
     * splice at the caret. */
    get usingPlus() {
      return plusMenuOpen;
    },
    close,
    togglePlus,
    onTyped() {
      dismissed = false;
    },
  };
}
