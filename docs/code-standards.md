# Haily — Code Standards

Guidelines for writing code in the Haily Rust workspace.

---

## Overview

Haily is a Rust workspace with 8 main crates under `crates/`. The Tauri frontend (`src-tauri/`) is a thin shell that invokes Rust commands. All business logic lives in crates and is reused by GUI, CLI, and headless modes.

---

## File & Module Organization

### Crate Structure

```
crates/
├── haily-db           ← schema & SQL queries (owned by this crate only)
├── haily-core         ← orchestrator, agent, feedback parsing
├── haily-kms          ← knowledge management, skill synthesis, feedback signals
├── haily-llm          ← LLM client abstraction (Anthropic, OpenAI, Gemini)
├── haily-tools        ← tool definitions & implementations
├── haily-cli          ← CLI entry point (reuses crates)
├── haily-io           ← Telegram & external integrations
└── haily-agent-proto  ← (proto definitions for sub-agents, if applicable)
```

### Module Principles

1. **One responsibility per file** — if a file exceeds ~200 lines, split it
2. **Public API at crate root** — re-export key types in `lib.rs`
3. **No cyclic dependencies** — use `haily_db::queries::*` abstraction to avoid tight coupling
4. **Test modules co-located** — `#[cfg(test)] mod tests { ... }` at end of file

### File Naming

- **Modules:** snake_case (e.g., `skill_synthesis.rs`, `feedback_parser.rs`)
- **Tests:** append `_tests.rs` or use `mod tests` (preferred)
- **Constants:** ALL_CAPS in SCREAMING_SNAKE_CASE

---

## Code Quality

### Compilation & Type Safety

- **No `unwrap()` in production code** — use `?` operator or `Result` propagation
- **No `.expect()`** — match errors or log explicitly
- **All code must compile** — `cargo check` before commit
- **No clippy warnings** — run `cargo clippy -- -D warnings`
- **Type annotations optional** — let type inference work, annotate complex paths only

### Error Handling

```rust
// Good: propagate with ?
pub async fn parse_feedback(msg: &str) -> Result<Option<FeedbackSignal>> {
    let lower = msg.to_lowercase();
    // ... work ...
    Ok(result)
}

// Good: explicit match with logging
match db_skills::insert_skill(...).await {
    Ok(skill) => info!(name = %skill.name, "skill saved"),
    Err(e) => warn!("skill insert failed: {e:#}"),
}

// Bad: unwrap in production
let skill = insert_skill(...).await.unwrap(); // ❌
```

### Comments

- **Comment the why, not the what** — code is self-documenting
- **Preconditions & invariants** — document non-obvious assumptions
- **Async contracts** — document what must happen before/after, cancellation safety
- **Public API** — always document params, return value, errors

```rust
/// Detect a feedback signal in the user's message.
/// Returns None if no signal is detected (e.g., plain chat message).
///
/// # Vietnamese & English patterns
/// Positive: 👍, tốt, hay, perfect, thank, good
/// Negative: 👎, sai, dài quá, wrong, bad
/// Correction: "không phải X mà là Y" or "not X but Y"
pub fn detect_feedback(msg: &str) -> Option<FeedbackSignal> { ... }
```

### Naming Conventions

| Category | Convention | Example |
|----------|-----------|---------|
| Functions | snake_case | `apply_feedback_signal()` |
| Types | PascalCase | `FeedbackSignal`, `TaskTrace` |
| Constants | SCREAMING_SNAKE_CASE | `EMA_ALPHA`, `DECAY_LAMBDA` |
| Modules | snake_case | `skill_synthesis`, `feedback_parser` |
| Variables | snake_case | `confidence`, `task_description` |
| Booleans | is_/has_/can_ prefix | `is_archived`, `has_high_confidence` |

### Performance & Efficiency

- **Avoid cloning**— use `&str`, `&[T]`, or `.clone()` only when necessary
- **Batch DB operations** — use `INSERT ... RETURNING *` over round-trips
- **Cache embeddings** — don't re-compute for same text
- **Preallocate vectors** — if size is known: `Vec::with_capacity(n)`

---

## Testing

### Test Organization

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jaccard_similarity() {
        assert_eq!(jaccard("hello world", "hello world"), 1.0);
        assert_eq!(jaccard("a b", "c d"), 0.0);
    }

    #[tokio::test]
    async fn test_synthesize_skills_from_traces() {
        let db = setup_test_db().await;
        let skills = synthesize_skills_from_traces(&db, mock_llm).await.unwrap();
        assert!(!skills.is_empty());
    }
}
```

### Coverage Requirements

- **Critical paths** (agent execution, skill synthesis, feedback) — 80%+ coverage
- **Error scenarios** — at least one happy path + one error path per function
- **Database operations** — test with real SQLite (use `setup_test_db()`)
- **LLM calls** — mock the LLM client (don't call real API in tests)

---

## Async & Concurrency

### Tokio Runtime

- **`#[tokio::main]`** for CLI entry point
- **`#[tokio::test]`** for async unit tests
- **Channels for worker communication** — skill synthesis & decay workers use background tasks

### Task Lifetime

- **Workers spawned in `Orchestrator::init()`** — must be long-lived, idempotent
- **Skill synthesis worker** — runs hourly, continues on LLM failure
- **Decay worker** — runs daily (24h after previous), archives stale skills
- **All workers must log progress** — use `tracing::info!()` and `tracing::warn!()`

### Cancellation Safety

Workers should handle shutdown gracefully:
```rust
// Good: periodic check for shutdown signal
loop {
    tokio::select! {
        _ = shutdown_signal.recv() => {
            info!("skill synthesis worker shutting down");
            break;
        }
        _ = tokio::time::sleep(Duration::from_secs(3600)) => {
            // Run synthesis...
        }
    }
}
```

---

## Database & Schema

### Schema Ownership

**haily-db** is the single source of truth for schema and queries:
- **No other crate defines tables** — migrate in `migrations/`
- **Query types live in `queries/*.rs`** — one file per table/feature
- **Use `FromRow` derives** — automatic row-to-struct mapping

### Query Conventions

```rust
// Good: explicit transaction boundaries
pub async fn insert_skill(
    db: &DbHandle,
    name: &str,
    description: &str,
    pattern: &str,
    steps_json: &str,
) -> Result<Skill> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, Skill>(
        "INSERT INTO kms_skills (id, name, description, pattern, steps, confidence, use_count, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 1.0, 0, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(name)
    .bind(description)
    .bind(pattern)
    .bind(steps_json)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?)
}
```

### Timestamps & UUIDs

- **Always RFC3339 UTC** for `created_at`, `updated_at`: `chrono::Utc::now().to_rfc3339()`
- **UUID v4** for all PKs: `uuid::Uuid::new_v4().to_string()`
- **Soft delete** — use `deleted_at: Option<String>`, never hard-delete

---

## Dependencies & Feature Flags

### Core Dependencies (Locked)

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
sqlx = { version = "0.7", features = ["sqlite", "chrono", "uuid"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
anyhow = "1.0"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

### Feature Flags (if applicable)

```toml
[features]
default = ["offline-llm"]
offline-llm = []     # Enable llama.cpp embedded
gpu-cuda = []        # CUDA support (requires CUDA toolkit)
gpu-metal = []       # Metal support (macOS)
```

- **Document feature usage** in README
- **No features alter public API** — features should be internal (e.g., inference backend)

---

## Documentation & Comments

### Public API Docs

All public types and functions require doc comments:

```rust
/// Represents a feedback signal from the user.
///
/// Used to adjust model behavior and store corrections for future reference.
#[derive(Debug, Clone)]
pub enum FeedbackSignal {
    /// User indicated satisfaction (👍, tốt, hay, etc.)
    Positive,
    /// User indicated dissatisfaction with optional topic.
    ///
    /// # Topics
    /// - "response_length" — response too long or too short
    /// - "language" — language preference issue
    /// - "tone" — inappropriate tone
    Negative { topic: Option<String> },
    /// User corrected something: "not X but Y"
    Correction { old: String, new: String },
}
```

### README per Crate

Each crate should have a minimal `README.md`:
```markdown
# haily-kms

Knowledge management system: skill synthesis, feedback processing, memory.

## Features
- Skill synthesis from task traces (Jaccard clustering + LLM)
- EMA confidence tracking
- Exponential decay & archival

## Stability
Stable. Used in production for skill management.
```

---

## Linting & CI

### Pre-Commit Checklist

Before committing, run:
```bash
cargo fmt              # Format code
cargo clippy -- -D warnings  # Lint (fail on warnings)
cargo test             # Run tests
cargo check            # Type check
```

### CI Expectations

- **All tests must pass** — commit will fail if tests fail
- **No clippy warnings** — even yellow warnings fail the build
- **Coverage tracking** — aim for 75%+ on modified files
- **No security warnings** — `cargo audit` must pass

---

## Convention Over Configuration

- **Errors propagate by default** — use `?` operator throughout
- **Logging goes to `tracing`** — use `info!()`, `warn!()`, `error!()`
- **Async is default** — all I/O is async; tests use `#[tokio::test]`
- **Tests are mandatory** — every function gets at least a happy-path test

---

## Breaking Changes

If you modify a public API:
1. **Update all callers in the workspace** — no stub implementations
2. **Document in `BREAKING.md`** (future release notes)
3. **Bump minor version** of affected crates (`Cargo.toml`)
4. **Update tests** to match new signature

---

## Related Documentation

- `architecture.md` — Technical decisions (8+ decisions documented)
- `project-overview-pdr.md` — Product design record
- `project-roadmap.md` — Phase timeline and completion status
- `project-structure.md` — Crate layout and module organization
