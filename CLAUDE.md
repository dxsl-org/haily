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
| `crates/haily-core/src/agent.rs` | Agent loop, tool dispatch, streaming, multi-turn state, kill-switch re-check |
| `crates/haily-core/src/approval.rs` | Session-bound tool approval broker (real implementation) |
| `crates/haily-core/src/budget.rs` | Token-budgeted context assembly (replaces 15-turn window) |
| `crates/haily-core/src/feedback_parser.rs` | Vietnamese + English feedback signal detection |
| `crates/haily-core/src/tag_matcher.rs` | Canonical tag hold-back for streaming (llama + cloud SSE) |
| `crates/haily-core/src/delegate.rs` | Sub-agent spawning, tier selection, shared memory |
| `crates/haily-core/src/tool_call.rs` | Tool dispatch, risk tier gating, kill-switch exemption logic |
| `crates/haily-kms/src/skills.rs` | Skill synthesis (Jaccard clustering), EMA confidence, exponential decay |
| `crates/haily-kms/src/feedback.rs` | FeedbackSignal enum (Positive, Negative, Correction) |
| `crates/haily-kms/src/hnsw.rs` | HNSW index w/ tombstones, dump/load persistence, atomic swap |
| `crates/haily-llm/src/router.rs` | LLM routing: llama.cpp primary → cloud fallback |
| `crates/haily-llm/src/sse.rs` | SSE parser for cloud streaming (OpenAI, Anthropic) |
| `crates/haily-llm/src/breaker.rs` | Circuit breaker (real file; not circuit_breaker.rs) |
| `crates/haily-db/src/queries/journal.rs` | Action journal insert, readback, undo queries (Safe Operator Harness) |
| `crates/haily-db/src/queries/connectors.rs` | Connector manifest CRUD (Safe Operator Harness phase 4) |
| `crates/haily-db/src/queries/` | All SQL — one file per domain |
| `crates/haily-tools/src/lib.rs` | RiskTier enum, Tool trait, ApprovalGate trait (via haily-types re-export) |
| `crates/haily-tools/src/connector/manifest.rs` | Manifest schema (version, ops, risk_tier, compensability) |
| `crates/haily-tools/src/connector/http_connector_tool.rs` | Generic HTTP tool interpreting a manifest op (outbox, read-back diff) |
| `crates/haily-tools/src/connector/odoo_executor.rs` | Odoo-specific executor (execute_kw, fault classification, C4 cred-by-ref) |
| `crates/haily-tools/src/connector/executor.rs` | ConnectorExecutor trait, UnconfiguredExecutor placeholder |
| `crates/haily-tools/src/journal_undo/mod.rs` | JournalUndoTool (IrreversibleWrite, kill-switch-exempt) |
| `crates/haily-tools/src/journal_undo/reconcile.rs` | Reconciliation state machine (attempt_undo, refusal logic) |
| `crates/haily-tools/src/security.rs` | ssrf_guard_with_allowance (IP/CIDR pin, metadata block) |
| `crates/haily-types/src/lib.rs` | RiskTier, ApprovalGate trait (leaf crate, avoids layering inversion) |
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

**Adding tools:** Tools and connectors live in `crates/haily-tools/src/`. V1 tools in `v1/` are registered in `ToolRegistry::build_v1()`. Connector tools (generic HTTP + Odoo-specific) live in `crates/haily-tools/src/connector/` and are registered via `register_connectors()`. Journal undo tool in `crates/haily-tools/src/journal_undo/` is registered in `build_v1()`.

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

**Phase 1–13: Complete (2026-07-03)**
- Architecture Remediation Plan (Phase Rem): all 10 phases shipped to main (2026-07-02)
- Safe Operator Harness (Phase 13): RiskTier, ApprovalGate seam, action journal + undo + kill switch, connector manifests, Odoo CRM (2026-07-03)
- Red Team findings (32 total): all accepted, applied, verified
- Regression gates: `cargo clippy -- -D warnings && cargo test` passing

**Next phases:** Phase 14 (Voice/Multimodal), Phase 15 (Multi-Device Sync) — deferred pending UX validation.

## Docs

Full documentation is in `.docs/`:
- `architecture.md` — 18 technical decisions (Decisions 14–18 added 2026-07-03: RiskTier, ApprovalGate, journal, connectors, Odoo)
- `code-standards.md` — Rust coding conventions; CancellationToken + TaskTracker patterns; Safe Operator Harness patterns (append-only trigger, representation-normalizing read-back, seam via leaf trait)
- `project-structure.md` — 10-crate layout (haily-types, haily-app added), dependency graph, layering test
- `project-roadmap.md` — Phase status (1–13 complete; Phase 14–15 planned)
- `project-changelog.md` — Significant changes, features, fixes by phase (Safe Operator Harness added 2026-07-03)
