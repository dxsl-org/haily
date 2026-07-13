# Haily — Codebase Guide for Claude

**Use Fable subagents when you need more intelligence**

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
| `crates/haily-core/src/agent/` | Agent loop split into modules — turn.rs (run_turn, TurnRuntime), sub_turn.rs (run_sub_turn, delegation memory), stream.rs (streaming + tag hold-back), outcome.rs (TaskOutcome→EMA recording, approval stats); mod.rs re-exports the public API |
| `crates/haily-core/src/approval.rs` | Session-bound tool approval broker (real implementation) |
| `crates/haily-core/src/budget.rs` | Token-budgeted context assembly (replaces 15-turn window) |
| `crates/haily-core/src/feedback_parser.rs` | Vietnamese + English feedback signal detection |
| `crates/haily-core/src/tag_matcher.rs` | Canonical tag hold-back for streaming (llama + cloud SSE) |
| `crates/haily-core/src/routing.rs` | Tier decision core (Auto Model Routing R1): deterministic 5-rung ladder selects tier from explicit intent phrases, message heuristics, depth mode, history length, and session default |
| `crates/haily-core/src/tier_intent.rs` | Explicit tier-intent phrase detection (VN/EN) with source-guard anchoring; feeds tier decision ladder |
| `crates/haily-core/src/delegate.rs` | Sub-agent spawning, tier selection, shared memory |
| `crates/haily-core/src/tool_call.rs` | Tool dispatch, risk tier gating, kill-switch exemption logic |
| `crates/haily-core/src/view/store.rs` | In-memory ViewStore (cap-bounded FIFO eviction, View Engine Phase A): holds DataView snapshots keyed by view_id; implements ViewSink trait |
| `crates/haily-kms/src/skills.rs` | Skill synthesis (Jaccard clustering), EMA confidence, exponential decay |
| `crates/haily-kms/src/feedback.rs` | FeedbackSignal enum (Positive, Negative, Correction) |
| `crates/haily-kms/src/hnsw.rs` | HNSW index w/ tombstones, dump/load persistence, atomic swap; contains/un_tombstone primitives for memory-undo |
| `crates/haily-kms/src/search.rs` | Hybrid recall: BM25 + HNSW-ANN fusion, per-channel relevance thresholds (default off, measure-first), recency tie-break |
| `crates/haily-kms/src/voice_check.rs` | Deterministic persona/voice-consistency eval (no LLM-judge) — model-upgrade drift gate |
| `crates/haily-llm/src/router.rs` | LLM routing: llama.cpp primary → cloud fallback |
| `crates/haily-llm/src/sse.rs` | SSE parser for cloud streaming (OpenAI, Anthropic) |
| `crates/haily-llm/src/breaker.rs` | Circuit breaker (real file; not circuit_breaker.rs) |
| `crates/haily-db/src/queries/journal.rs` | Action journal insert, readback, undo queries (Safe Operator Harness) |
| `crates/haily-db/src/queries/routing_decisions.rs` | Routing decision telemetry insert, list, and summary queries (Auto Model Routing R1) — the R2/R3 training set |
| `crates/haily-db/src/queries/connectors.rs` | Connector manifest CRUD (Safe Operator Harness phase 4) |
| `crates/haily-db/src/queries/` | All SQL — one file per domain |
| `crates/haily-db/src/queries/view_events.rs` | View Engine telemetry funnel (View Engine Phase A): presented/viewed/projection_switched/usefulness/edit_demand events + DB-level anti-false-positive guard (drops empty-detail edit_demand rows) |
| `crates/haily-db/src/recurrence.rs` | RecurrenceRule engine + next_after (strict forward-progress) + occurrences_in_window; shared by reminders (proactive) and calendar |
| `crates/haily-tools/src/lib.rs` | RiskTier enum, Tool trait, ApprovalGate trait (via haily-types re-export) |
| `crates/haily-tools/src/connector/manifest/` | Manifest schema v2 (schema.rs: version, auth, protocol, ops, risk_tier, compensability; diff.rs: re-approval diff) |
| `crates/haily-tools/src/connector/protocol/` | Declarative protocol interpreter (envelope/arg/fault/read-back substitution, connection overlay) |
| `crates/haily-tools/src/connector/http_connector_tool.rs` | Generic HTTP executor interpreting any manifest op (host-scoped auth injection, protocol translation, outbox, read-back diff) |
| `crates/haily-tools/src/connector/executor.rs` | ConnectorExecutor trait, implementations for HTTP (generic) |
| `crates/haily-tools/src/journal_undo/mod.rs` | JournalUndoTool (IrreversibleWrite, kill-switch-exempt) |
| `crates/haily-tools/src/journal_undo/reconcile.rs` | Reconciliation state machine (attempt_undo, refusal logic) |
| `crates/haily-tools/src/view_present/` | present_view tool (View Engine Phase A): depth-0 quarantine guard, parse-then-repair args validation, RiskTier::Read; GBNF projection grammar builders in schema.rs |
| `crates/haily-tools/src/security.rs` | ssrf_guard_with_allowance (IP/CIDR pin, metadata block) |
| `crates/haily-types/src/lib.rs` | RiskTier, ApprovalGate trait, View Engine wire types (DataView/FieldType/ProjectionKind/ViewProvenance/ViewSink), Request/DepthMode/RunEvent/Notification (leaf crate, avoids layering inversion) |
| `crates/haily-io/src/lib.rs` | Adapter trait definition, manager |
| `crates/haily-app/src/bootstrap.rs` | Shared bootstrap (LlmConfig, db, orchestrator) |
| `crates/haily-app/src/dispatch.rs` | Mode dispatch (GUI/CLI/headless) + adapter wiring |
| `crates/haily-app/src/watchers.rs` | Signal handlers (Windows + Unix), graceful shutdown, worker spawns (rollup, backup, proactive) |
| `crates/haily-app/src/connector_config/` | Connector config admin (summary read, credential→keyring set with WAL scrub, status) — surfaced in GUI |
| `crates/haily-proactive/src/morning_brief.rs` | Morning brief with cross-domain synthesis (task↔calendar↔reminder↔memory) |
| `crates/haily-proactive/src/cross_domain/` | Cross-domain nudges + persistent cooldown ledger (survives restart) |
| `crates/haily-proactive/src/backup/` | GFS SQLite backup worker (daily/weekly/monthly, configurable retention), plaintext-credential scrub of the copy |
| `crates/haily-tools/src/schedule/` | VN/EN natural-language schedule parser (feeds reminder_add) |
| `crates/haily-io/src/proactive_cards.rs` | Typed proactive card model + per-kind coalesce/eviction for the GUI panel |
| `src-tauri/src/lib.rs` | Tauri app initialization (Tauri glue only) |
| `src/lib/tauri.ts` | Typed Tauri invoke wrappers |
| `src/lib/data-view.ts` | View Engine formatting/validation helpers (View Engine Phase A): safeHref scheme-allowlist gate for Url/Email/Phone attribute sinks (http/https/mailto/tel only; rejects javascript:/data:), sanitization utilities |
| `src/lib/components/` | GUI panels: WorkItemsPanel, ProactivePanel, JournalBrowser, ConnectorConfig + settings tabs |
| `src/lib/components/view/` | View Engine GUI pane (View Engine Phase A): WorkspacePane, ViewTable, ViewCards, ViewCell, ProjectionSwitcher, ViewMeasurementBar components + generic renderers for FieldType |
| `crates/haily-core/src/pipeline/launcher/` | Pipeline launcher service (Pipeline Activation phase 1): `Orchestrator::launch_coding_run` constructs PipelineRunner from orchestrator handles, wires RunEvent/distillation bridges to live runs, resolves target-repo path + verifier commands; split into mod.rs (launcher), registry.rs (tool/verifier helpers), tests.rs |
| `crates/haily-app/src/trigger.rs` | Dispatch-layer trigger resolver (Pipeline Activation phase 2): resolves Request → TriggerAction (NormalTurn / LaunchPlan / LaunchBuild / ConfirmThenLaunch / PromptTask / UnknownSlashHint); handles confirm-gated launch flow via ApprovalGate broker; runs as part of turn execution, never blocks dispatch loop |
| `crates/haily-core/src/coding_intent.rs` | Chat-intent classifier (Pipeline Activation phase 2): VN/EN pattern detection for pipeline auto-launch (`classify(msg, origin) -> Option<RunKind>`); reuses `feedback_parser` anchoring rules (phrases must be short or leading, never buried in long text); narrow multi-word lexicon (e.g. "build this feature", "fix this bug") to minimize false positives |
| `crates/haily-app/src/reaper.rs` | Worktree GC worker (Pipeline Activation phase 6): hourly background task reconciling coding_workspaces DB rows + on-disk git worktrees against pipeline run status; reaps terminal runs (past grace window) + aged NULL-run workspaces + crash orphans; best-effort (logs errors, continues tick); graceful shutdown via `tokio::select!` |
| `crates/haily-kms/src/skill_gates.rs` | Skill enable/pin enforcement loader (Pipeline Activation phase 5): reads persisted enable/pin admin state from `meta` prefs table (`skill.enabled.<name>` / `skill.pinned.<name>`), returns `SkillGates` for injection filtering; fail-open (DB read error yields default empty gates) so admin state never blocks turn context assembly |

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

**Adding tools:** Tools and connectors live in `crates/haily-tools/src/`. V1 tools in `v1/` are registered in `ToolRegistry::build_v1()`. Connector tools use a single generic `HttpExecutor` (lives in `crates/haily-tools/src/connector/http_connector_tool.rs`) that interprets manifest v2 schema for any connector; connectors are registered via `register_connectors()` which reads manifests from the DB. Journal undo tool in `crates/haily-tools/src/journal_undo/` is registered in `build_v1()`.

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

All shipped phases and what's next live in `docs/project-roadmap.md` (authoritative, updated per merge) — this file tracks architecture and code layout, not a phase timeline, so it doesn't duplicate that table here.

Regression gate for every merge: `cargo clippy -- -D warnings && cargo test` passing.

## Docs

Full documentation is in `docs/`:
- `architecture.md` — technical decisions log, one entry per significant architectural choice
- `code-standards.md` — Rust coding conventions; CancellationToken + TaskTracker patterns; Safe Operator Harness patterns (append-only trigger, representation-normalizing read-back, seam via leaf trait, out-param side-channel, credential-getter seam)
- `project-structure.md` — crate layout, dependency graph, layering test
- `project-roadmap.md` — phase status, in-flight plans, what's next
- `project-changelog.md` — Significant changes, features, fixes by phase (Harness Completion Phase 5 added 2026-07-04)
