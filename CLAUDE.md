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
| `crates/haily-db/src/recurrence.rs` | RecurrenceRule engine + next_after (strict forward-progress) + occurrences_in_window; shared by reminders (proactive) and calendar |
| `crates/haily-tools/src/lib.rs` | RiskTier enum, Tool trait, ApprovalGate trait (via haily-types re-export) |
| `crates/haily-tools/src/connector/manifest/` | Manifest schema v2 (schema.rs: version, auth, protocol, ops, risk_tier, compensability; diff.rs: re-approval diff) |
| `crates/haily-tools/src/connector/protocol/` | Declarative protocol interpreter (envelope/arg/fault/read-back substitution, connection overlay) |
| `crates/haily-tools/src/connector/http_connector_tool.rs` | Generic HTTP executor interpreting any manifest op (host-scoped auth injection, protocol translation, outbox, read-back diff) |
| `crates/haily-tools/src/connector/executor.rs` | ConnectorExecutor trait, implementations for HTTP (generic) |
| `crates/haily-tools/src/journal_undo/mod.rs` | JournalUndoTool (IrreversibleWrite, kill-switch-exempt) |
| `crates/haily-tools/src/journal_undo/reconcile.rs` | Reconciliation state machine (attempt_undo, refusal logic) |
| `crates/haily-tools/src/security.rs` | ssrf_guard_with_allowance (IP/CIDR pin, metadata block) |
| `crates/haily-types/src/lib.rs` | RiskTier, ApprovalGate trait (leaf crate, avoids layering inversion) |
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
| `src/lib/components/` | GUI panels: WorkItemsPanel, ProactivePanel, JournalBrowser, ConnectorConfig + settings tabs |
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

**Phase 1–13.5: Complete (2026-07-04)**
- Architecture Remediation Plan (Phase Rem): all 10 phases shipped to main (2026-07-02)
- Safe Operator Harness (Phase 13): RiskTier, ApprovalGate seam, action journal + undo + kill switch, connector manifests, Odoo CRM (2026-07-03)
- Harness Completion (Phases 13.1–13.5): Local journal/undo, per-turn cap, human-verb UI, keyring credentials (dormant), TaskOutcome→EMA wiring, label sources + confidence weighting, anti-reinforcement invariants, golden-task eval harness (23 cases, zero network, behavior-asserting) (2026-07-04)
- Red Team findings (32 total): all accepted, applied, verified
- Regression gates: `cargo clippy -- -D warnings && cargo test` passing

**Sub-Agent & Skill Architecture (P0–P14): Complete (2026-07-12)**
- Full plan→build→verify→ship pipeline with sandbox isolation, multi-language coding tools, skill synthesis, LSP semantics, stealth browser, automation eval harness
- PR #6 merged to main

**Mobile Thin-Client: Complete (2026-07-12)**
- Android-first WS remote terminal with QR pairing, voice plugin, E2E harness
- iOS (P5) explicitly deferred, host-gated on macOS
- PR #7 merged to main

**Auto Model Routing R1: Complete (2026-07-14)**
- Heuristic tier selector + gate-verified escalation policy, routing decision telemetry, cost/quality UX dial
- PR #8 merged to main

**Pipeline Activation & Wiring (7 phases): Complete (2026-07-14)**
- Phase 1: Launcher service + live RunEvent/distillation bridges (`launch_coding_run` in core, `spawn_distillation_bridge` in app)
- Phase 2: Chat triggers — `/plan`, `/code`, `/build` slash + confirm-gated VN/EN intent classifier
- Phase 3: GUI cockpit "New run" form feeding RunTimeline
- Phase 4: LSP diagnostics into build fix-loop signal
- Phase 5: Skill enable/pin enforcement at context-assembly time
- Phase 6: Hourly worktree GC with graceful-shutdown semantics + grace-window fix (commit ce21efd)
- Phase 7: Host-gated eval runbook scripts + documentation (`scripts/evals/`, `docs/runbooks/pipeline-evals.md`)
- Status: Built on `feat/pipeline-activation`, awaiting merge/push

**Next phases:** Phase 14 (Voice/Multimodal), Phase 15 (Multi-Device Sync) — deferred pending UX validation. R2/R3 router gated on ≥7 days routing_decisions data + P9 eval matrix.

## Docs

Full documentation is in `docs/`:
- `architecture.md` — 30 technical decisions (Decision 30 added 2026-07-14: Auto Model Routing R1 heuristic selector; Decisions 1–29 from earlier phases)
- `code-standards.md` — Rust coding conventions; CancellationToken + TaskTracker patterns; Safe Operator Harness patterns (append-only trigger, representation-normalizing read-back, seam via leaf trait, out-param side-channel, credential-getter seam)
- `project-structure.md` — 10-crate layout (haily-types, haily-app added), dependency graph, layering test
- `project-roadmap.md` — Phase status (1–13.5 complete; 13.1–13.5 collapsed into single summary; Phase 14–15 planned; follow-up directions: generic declarative connectors + router A/B)
- `project-changelog.md` — Significant changes, features, fixes by phase (Harness Completion Phase 5 added 2026-07-04)
