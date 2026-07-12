---
name: lang-typescript
description: TypeScript / JavaScript standards — types at boundaries, JSDoc, async contracts, idioms to follow when writing or reviewing TS/JS.
when_to_use: When writing, reviewing, or fixing TypeScript or JavaScript code.
domain: developer
kind: standard
specialists: []
---

# TypeScript / JavaScript Standards

## Types

- Prefer `unknown` over `any` at system boundaries; narrow with type guards. Every `any` needs a justifying comment.
- `interface` for object shapes; `type` for unions, intersections, mapped types.
- Use `satisfies` over `as` for narrowing when possible. A non-null `!` or `as` cast that is not obvious gets a `// SAFETY:` comment explaining the invariant.
- Prefer `const` assertions (`as const`) over ad-hoc enums for small fixed sets.

## Docs

- JSDoc on exported symbols; omit it for internal functions whose name + signature are self-documenting.
- `@param` only when the type alone is ambiguous. Always document `@throws` for domain errors.
- Document cancellation/ordering for async functions when not obvious.

## Anchor comments

- `// NOTE:` invariant a reader must know · `// TODO: owner/issue` · `// FIXME: owner/issue` (always with a ticket ref).

## Idioms

- Handle every rejected promise — an unhandled rejection is a crash. Document whether a `Promise<void>` can reject.
- Validate external input at the edge; do not trust request/response shapes.
- Prefer immutable data and pure functions; isolate side effects.
- Lint + typecheck (`tsc --noEmit`) must be clean before done.
