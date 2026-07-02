# Haily — Codebase Guide for Claude

Haily is a local-first, always-on AI assistant built as a single Rust binary + Tauri/Svelte desktop app. All data stays on-device. Three deployment modes share one binary: `--gui` (Tauri), `--cli` (terminal REPL), `--headless` (background daemon).

## Build & Run

```powershell
# Type check entire workspace
cargo check

# Lint (fail on warnings)
cargo clippy -- -D warnings

# Format
cargo fmt

# Run all tests
cargo test

# Build release binary
cargo build --release

# Dev frontend (Svelte)
npm run dev

# Tauri dev (GUI mode)
npm run tauri dev

# Run GPU build
cargo build --release --features gpu-cuda   # CUDA
cargo build --release --features gpu-metal  # Metal (macOS)
```

## Architecture

```
haily-llm    (leaf — LLM provider abstraction)
haily-db     (leaf — SQLite schema + typed queries)
haily-io     (leaf — Adapter trait: GUI, CLI, Telegram...)
     ↑
haily-kms    (KMS: memory, skills, feedback)
haily-tools  (tool definitions + implementations)
     ↑
haily-core   (Orchestrator, agent loop, streaming)
     ↑
haily-proactive  (background daemon, morning brief)
haily-cli    (binary entry point: --cli / --headless)
src-tauri    (Tauri shell: GUI entry + IPC bridge)
```

**Critical rule:** `haily-core` must never import from `haily-io`. They communicate through tokio mpsc channels only.

## Key Files

| File | Purpose |
|------|---------|
| `crates/haily-core/src/orchestrator.rs` | Single entry point for all requests |
| `crates/haily-core/src/agent.rs` | Agent loop, streaming, tool dispatch |
| `crates/haily-kms/src/skills.rs` | Skill synthesis, EMA confidence, decay |
| `crates/haily-kms/src/feedback.rs` | FeedbackSignal enum |
| `crates/haily-llm/src/router.rs` | llama.cpp → Ollama → cloud routing |
| `crates/haily-db/src/queries/` | All SQL — one file per domain |
| `crates/haily-io/src/lib.rs` | Adapter trait definition |
| `src-tauri/src/commands/` | Tauri IPC bridge (thin wrappers only) |
| `src/lib/tauri.ts` | Typed Tauri invoke wrappers |

## Code Standards

**Error handling:** `?` everywhere. No `unwrap()` or `expect()` in production code.

**Async:** Tokio runtime. Workers spawned in `Orchestrator::init()`. Always handle shutdown via `tokio::select!`.

**Database:** All SQL lives in `haily-db/src/queries/`. UUIDs as PKs, RFC3339 timestamps, soft-delete with `deleted_at`.

**Naming:**
- Functions/modules/variables: `snake_case`
- Types: `PascalCase`
- Constants: `SCREAMING_SNAKE_CASE`
- Booleans: `is_/has_/can_` prefix

**Comments:** Document *why* and contract, not *what*. Public API requires doc comments with params/returns/errors.

**Adding tools:** New file in `crates/haily-tools/src/v2/` + register in `registry.rs`.

**Adding I/O adapters:** New module in `crates/haily-io/src/` + wire up at app layer only.

## Self-Improvement Loop (Phase 11 — Complete)

- Hourly: cluster task traces by Jaccard similarity → LLM synthesizes skill → inject screen → save
- Daily: exponential decay `λ=0.693/24h` on all skills; archive `confidence < 0.30`
- EMA confidence: `α=0.10` updated on task success/failure
- Feedback detection: Vietnamese + English patterns in user messages

## Current Phase

**Phase 12 — Agentic Optimization (In Progress)**
Smart LLM routing by task complexity/cost/latency, sub-agent lifecycle management.

Next phases: Phase 13 (Voice/Multimodal), Phase 14 (Multi-Device Sync).

## Docs

Full documentation is in `.docs/`:
- `architecture.md` — 8 technical decisions with rationale
- `code-standards.md` — Rust coding conventions
- `project-structure.md` — Crate layout and dependency graph
- `project-roadmap.md` — Phase status and milestones
- `sub-agent-protocol.md` — V1/V2 agent hierarchy design
- `voice-spec.md` — Personality constants and 4 souls system
