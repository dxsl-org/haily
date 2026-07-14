import { defineConfig } from 'vitest/config';

// Pure-TS unit tests only (no DOM, no Svelte components) — `src/lib/data-view.test.ts` and
// siblings. Deliberately its own minimal config rather than extending `vite.config.ts`: the
// SvelteKit vite plugin isn't needed here since these tests use relative imports, not the
// `$lib` alias, and pulling it in would slow every test run for no benefit.
export default defineConfig({
  test: {
    include: ['src/**/*.test.ts'],
    environment: 'node',
  },
});
