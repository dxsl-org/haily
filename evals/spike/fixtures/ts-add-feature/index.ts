// SPIKE FIXTURE (throwaway). Task: implement `slugify` so index.test.ts passes.
// The feature does not exist yet — it throws. This is the "tiny feature with tests" case.

/**
 * Convert a string to a URL slug: lowercase, non-alphanumeric runs collapsed to single
 * hyphens, leading/trailing hyphens trimmed. See index.test.ts for the exact contract.
 */
export function slugify(_input: string): string {
  throw new Error("not implemented");
}
