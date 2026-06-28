# Haily — Project Changelog

Significant changes, features, and fixes by phase.

---

## [Unreleased]

### Improvements
- Self-improvement loop for skill synthesis and confidence tracking
- Exponential decay mechanism for stale skills
- Multi-language feedback detection (Vietnamese + English)

---

## [Phase 11] — Self-Improvement Loop — 2026-06-28

### Improvements
- **Skill synthesis**: Jaccard clustering of task traces → LLM generalize → injection screening → save to DB
- **EMA confidence (α=0.10)**: Per-skill confidence updated on success/failure
- **Exponential decay (λ=0.693/24h, archive<0.30)**: Skills decay with time to avoid stale patterns
- **Feedback detection**: Vietnamese + English patterns; detects 👍/👎 and corrections
- **Per-turn trace recording**: Task traces saved to DB for skill synthesis
- **FeedbackReactTool**: Explicit tool LLM can call when user gives feedback
- **Self-improvement workers**: Hourly skill synthesis, daily skill decay spawned in Orchestrator::init()

### Architecture
- **FeedbackSignal enum** in `haily-kms::feedback`: Positive, Negative{topic}, Correction{old,new}
- **Feedback parser** in `haily-core::feedback_parser`: Scans user messages for signal patterns
- **Skill module** in `haily-kms::skills`: Synthesis, EMA update, exponential decay
- **KMS task traces table**: Records per-turn task description, tool calls, outcome, duration

---

## [Phase 10] — Memory & Knowledge Integration — 2026-06-15

### Improvements
- Unified knowledge management system (KMS) for memory synthesis
- Graph-augmented retrieval for episodic and semantic memory
- Per-session memory isolation

---

## [Phase 1–9] — Foundation & Core Features

### Improvements
- Rust-based single binary (GUI via Tauri + Svelte, CLI, headless daemon)
- Local inference via llama.cpp (embedded) with Ollama optional enhancement
- SQLite local storage with sync-friendly schema (UUID, soft delete, timestamps)
- Telegram integration for headless/remote communication
- Vector search via HNSW with multilingual embeddings
- Feedback loop foundation for user preference tracking

---

## Convention

- **Improvements**: New features, enhancements, optimizations
- **Fixes**: Bug fixes, patches, security updates
- **Changed**: Breaking changes, API modifications
- **Deprecated**: Functionality marked for removal

Each entry lists concrete deliverables that shipped in that phase.
