# Changelog

All notable changes to Haily are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

---

## [Unreleased]

### Added
- db: action journal and undo for local tools
- tools: generic snapshot compensator for tasks/notes/reminders
- core: per-turn destructive-op cap with approval escalation
- core: turn_id groups a turn's writes for undo
- gui: human-verb approval cards with inline undo
- app: OS-keyring credential storage with fallback
- tools: manifest version-drift re-approval gate
- kms: task outcome drives skill confidence
- db: per-turn telemetry columns and daily rollup
- core: deterministic offline golden-task eval harness
- core: three-tier agent delegation hierarchy
- core: delegate tools for six domains
- core: stateless sub-agent turn execution
- tools: per-tier tool whitelist sub-registry
- agent: work item tracking with checkpoints
- agent: ephemeral worktree sandbox for tasks
- cli: work item status panel above prompt

### Security
- kms: unknown outcomes never move confidence
- kms: archival requires two independent negatives
- core: feedback downgrade parses only user message
- app: plaintext credential write fails closed
- core: block approval tools inside sub-agents
- core: strip tool markup from results

### Fixed
- db: guard FTS5 triggers against index corruption
- core: loop guard terminates runaway turns
- telegram: handle work items changed notification

### Changed
- tools: soft-delete tools re-tiered to reversible
- core: sub-turns record traces without moving confidence
- db: calendar and facts use param structs

---

## [0.1.0-beta] — Phase 11: Self-Improvement Loop — 2026-06-28

### Added
- **Skill synthesis worker** (hourly): Jaccard clustering of task traces → LLM generalize → injection screening → save to `kms_skills`
- **Skill decay worker** (daily): exponential decay `λ=0.693/24h`; archive skills with `confidence < 0.30`
- **EMA confidence tracking** (`α=0.10`): per-skill confidence updated on task success/failure
- **FeedbackSignal enum** (`haily-kms::feedback`): `Positive`, `Negative{topic}`, `Correction{old,new}`
- **Feedback parser** (`haily-core::feedback_parser`): detects Vietnamese + English feedback patterns including 👍/👎 and corrections
- **FeedbackReactTool**: explicit tool the LLM can call when feedback is detected
- **Per-turn task trace recording**: `kms_task_traces` table captures description, tool calls, outcome, duration
- **Injection screening**: blocks BLOCKED_PHRASES and strips control characters from synthesized skills
- **Multi-key cloud API** with round-robin rotation and 429 failover (`haily-llm`)
- **Gemma4/ChatML prompt format** support with configurable embedded model
- **GPU auto-detection**: compile-time feature flags for CUDA/Metal layer offloading
- **Settings panel** (UI): LLM model config, persona selection, gear icon

### Fixed
- Strip trailing stop tokens leaking into Gemma4/ChatML output
- Embedded llama.cpp inference broken after workspace restructure
- Self-improvement loop review findings from code audit

### Architecture
- `FeedbackSignal` enum in `haily-kms::feedback`
- `detect_feedback()` in `haily-core::feedback_parser`
- Synthesis + decay + EMA in `haily-kms::skills`
- DB: `kms_skills`, `kms_task_traces` tables in `migrations/0003_kms_memory.sql`
- Both workers spawned as background tasks in `Orchestrator::init()`

---

## [0.0.9-beta] — Phase 10: Memory & Knowledge Integration — 2026-06-15

### Added
- Unified Knowledge Management System (KMS) — episodic and semantic memory
- Graph-augmented retrieval for memory search
- Per-session memory isolation
- HNSW vector index (`hnsw_rs`) with `multilingual-e5-base` embeddings (768 dims)
- `fastembed-rs` for in-process embedding generation (no Ollama dependency for embeddings)

---

## [0.0.1-beta] — Phases 1–9: Foundation & Core — 2026-05

### Added
- Rust workspace foundation with 8 crates (`haily-core`, `haily-kms`, `haily-llm`, `haily-db`, `haily-tools`, `haily-io`, `haily-proactive`, `haily-cli`)
- Single binary with three deployment modes: `--gui`, `--cli`, `--headless`
- Tauri 2.0 + Svelte 5 + shadcn-svelte desktop GUI
- llama.cpp embedded as primary local inference (offline-first, `qwen2.5:3b` default)
- Ollama HTTP API integration as optional enhancement
- Cloud API clients: Anthropic, OpenAI, Gemini with LLM routing
- SQLite local storage with sync-friendly schema (UUID PKs, soft delete, RFC3339 timestamps)
- Telegram adapter for headless/remote communication
- Unified I/O Adapter abstraction (`haily-io`): GUI, CLI, Telegram share one interface
- `haily-proactive`: background daemon, morning brief, cross-domain pattern detection
- Feedback loop foundation for user preference tracking
- `haily-tools` v1: web search, calendar, notes, reminders, tasks, memory tools
- Circuit breaker in `haily-llm`: 3 failures → open circuit, 30s probe
