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
haily-types  (leaf — Request, ResponseChunk, ApprovalResolver trait)
    ↑
haily-llm    (leaf — LLM routing, streaming, breaker)
haily-db     (leaf — SQLite schema + typed queries, sqlx 0.8)
haily-io     (leaf — Adapter trait: GUI, CLI, Telegram)
    ↑
haily-kms    (KMS: memory, HNSW persistence, skills, feedback)
haily-tools  (tool definitions + implementations, v1)
    ↑
haily-core   (Agent orchestrator, approval broker, budget, context)
    ↑
haily-app    (Shared bootstrap, dispatch, graceful shutdown)
haily-proactive  (Background daemon, morning brief, reminders)
haily-cli    (Binary entry point: thin wrapper around haily-app)
src-tauri    (Tauri shell: 119 lines, GUI glue only)
```

**Critical rule:** `haily-core` NEVER imports `haily-io`. They communicate exclusively via `haily-types` messages over tokio mpsc channels. Layering test enforces at CI.

## Key Files

| File | Purpose |
|------|---------|
| `crates/haily-core/src/lib.rs` | Orchestrator export, module coordination |
| `crates/haily-core/src/agent.rs` | Agent loop, tool dispatch, streaming, multi-turn state |
| `crates/haily-core/src/approval.rs` | Session-bound tool approval broker (real implementation) |
| `crates/haily-core/src/budget.rs` | Token-budgeted context assembly (replaces 15-turn window) |
| `crates/haily-core/src/feedback_parser.rs` | Vietnamese + English feedback signal detection |
| `crates/haily-core/src/tag_matcher.rs` | Canonical tag hold-back for streaming (llama + cloud SSE) |
| `crates/haily-core/src/delegate.rs` | Sub-agent spawning, tier selection, shared memory |
| `crates/haily-kms/src/skills.rs` | Skill synthesis (Jaccard clustering), EMA confidence, exponential decay |
| `crates/haily-kms/src/feedback.rs` | FeedbackSignal enum (Positive, Negative, Correction) |
| `crates/haily-kms/src/hnsw.rs` | HNSW index w/ tombstones, dump/load persistence, atomic swap |
| `crates/haily-llm/src/router.rs` | LLM routing: llama.cpp primary → cloud fallback |
| `crates/haily-llm/src/sse.rs` | SSE parser for cloud streaming (OpenAI, Anthropic) |
| `crates/haily-llm/src/breaker.rs` | Circuit breaker (real file; not circuit_breaker.rs) |
| `crates/haily-db/src/queries/` | All SQL — one file per domain |
| `crates/haily-io/src/lib.rs` | Adapter trait definition, manager |
| `crates/haily-app/src/bootstrap.rs` | Shared bootstrap (LlmConfig, db, orchestrator) |
| `crates/haily-app/src/dispatch.rs` | Mode dispatch (GUI/CLI/headless) + adapter wiring |
| `crates/haily-app/src/watchers.rs` | Signal handlers (Windows + Unix), graceful shutdown |
| `src-tauri/src/lib.rs` | Tauri app initialization (Tauri glue only) |
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

## Self-Improvement Loop (Phases 11–12 Complete)

**Phase 11 — Skill Synthesis & Decay:**
- Hourly: cluster task traces by Jaccard similarity (0.40 threshold) → LLM synthesizes skill
- Save skill w/ initial confidence 1.0; EMA updates on use (α=0.10)
- Daily: exponential decay `λ=0.693/24h`; archive when `confidence < 0.30`
- Feedback detection: Vietnamese + English patterns in user messages

**Phase 12 — Agentic Optimization (Complete):**
- Smart LLM routing by task complexity/cost/latency (tier-based: fast/medium/thinking/ultra)
- Sub-agent lifecycle mgmt (spawn, memory sharing, tier selection)
- Delegation completion: multi-turn sub-agent work with local memory, live router reloads

## Current Phase Status

**Phase 1–12: Complete (2026-07-02 remediation cycle)**
- Architecture Remediation Plan: all 10 phases shipped to main
- Red Team findings (32 total): all accepted, applied, verified
- Regression gates: `cargo clippy -- -D warnings && cargo test` passing

**Next phases:** Phase 13 (Voice/Multimodal), Phase 14 (Multi-Device Sync) — deferred pending UX validation.

## Docs

Full documentation is in `.docs/`:
- `architecture.md` — 13 technical decisions (Decisions 9–12 added 2026-07-02)
- `code-standards.md` — Rust coding conventions, CancellationToken + TaskTracker patterns
- `project-structure.md` — 10-crate layout (haily-types, haily-app added), dependency graph, layering test
- `project-roadmap.md` — Phase status (1–12 complete as of 2026-07-02)
- `code-standards.md` — also documents real patterns (graceful shutdown, HNSW persistence)
