---
name: lang-rust
description: Rust coding standards — error handling, docs, unsafe, clippy, naming conventions to follow when writing or reviewing Rust.
when_to_use: When writing, reviewing, or fixing Rust code.
domain: developer
kind: standard
specialists: []
---

# Rust Standards

## Error handling

- Use `?` for propagation. No `unwrap()` / `expect()` on fallible paths in production code.
- Prefer `expect("states the invariant")` over bare `unwrap()` when a panic truly cannot happen — the message documents why.
- Fail-closed at boundaries: validate external input before acting on it.

## Docs

- `///` on every public item (fn, struct, enum, trait, module). `//!` at the top of `lib.rs`/`mod.rs`.
- Document the WHY and the contract (params, returns, errors), not the WHAT. Include `# Errors` / `# Panics` / `# Safety` only when they apply.
- Private items: comment only when intent is non-obvious.

## Unsafe

- A `// SAFETY:` comment is MANDATORY immediately before every `unsafe` block, stating the invariant the compiler cannot check. No unsafe without it, ever.

## Attributes

- Every `#[allow(...)]` carries an inline reason: `#[allow(clippy::too_many_arguments)] // reason: ...`.

## Idioms

- Naming: `snake_case` fns/vars/modules, `PascalCase` types, `SCREAMING_SNAKE_CASE` consts. Booleans prefixed `is_`/`has_`/`can_`.
- Prefer borrowing over cloning; clone deliberately, not to silence the borrow checker.
- Model illegal states as unrepresentable (enums over bool flags).
- `cargo clippy -- -D warnings` and `cargo fmt` must be clean before done.

## Async (tokio)

- Every long-running task handles shutdown via `tokio::select!` on a cancellation token.
- Never hold a `std::sync` lock across an `.await`.
