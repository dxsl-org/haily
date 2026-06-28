# Haily — Project Roadmap

Phase timeline, status, and key dependencies.

---

## Phase Summary

| Phase | Title | Status | Target | Shipped |
|-------|-------|--------|--------|---------|
| 1–9 | Foundation & Core Features | Complete | 2026-05 | Rust single binary, Tauri GUI, SQLite, feedback foundation |
| 10 | Memory & Knowledge Integration | Complete | 2026-06-15 | KMS, graph-augmented retrieval, per-session memory |
| 11 | Self-Improvement Loop | **Complete** | 2026-06-28 | Skill synthesis, EMA confidence, decay workers, feedback tools |
| 12 | Agentic Optimization (Planned) | In Progress | 2026-07 | Smart routing, cost optimization, sub-agent lifecycle |
| 13 | Voice & Multimodal (Planned) | Backlog | 2026-08 | Whisper STT, image analysis, voice output |
| 14 | Multi-Device Sync (Planned) | Backlog | 2026-09 | File-based or self-hosted sync, conflict resolution |

---

## Phase 11 — Self-Improvement Loop

**Status:** Complete  
**Shipped:** 2026-06-28

### Overview
Automated skill synthesis from task traces, confidence tracking via EMA, and decay workers to keep the skill library fresh and relevant.

### Key Deliverables
- Jaccard-based clustering of task traces (no embeddings needed)
- LLM-driven skill generalization with injection screening
- EMA confidence updates (α=0.10) per skill on success/failure
- Exponential decay (λ=0.693/24h) with archival threshold (< 0.30)
- Feedback signal detection (Vietnamese + English)
- Hourly skill synthesis worker
- Daily skill decay worker

### Related Code
- `crates/haily-kms/src/skills.rs` — synthesis, EMA, decay
- `crates/haily-kms/src/feedback.rs` — FeedbackSignal enum
- `crates/haily-core/src/feedback_parser.rs` — feedback detection
- `crates/haily-db/src/queries/skills.rs` — schema & queries
- `crates/haily-db/migrations/0003_kms_memory.sql` — kms_skills, kms_task_traces tables

---

## Phase 12 — Agentic Optimization (Planned)

**Status:** In Progress  
**Target:** 2026-07

### Overview
Smart routing of requests to appropriate LLM models/agents based on task complexity, cost optimization, and managed agent lifecycle.

### Key Decisions Pending
- Sub-agent spawning strategy (work-queue vs reactive)
- Model routing heuristics (cost, latency, complexity signals)
- Agent lifecycle management (cleanup, resource limits)

---

## Phase 13 — Voice & Multimodal (Planned)

**Status:** Backlog  
**Target:** 2026-08

### Overview
Speech-to-text (Whisper), image analysis, and voice output.

### Key Decisions Pending
- Whisper local vs cloud API trade-off
- Image analysis for task context enrichment
- Text-to-speech integration (local or cloud)

---

## Phase 14 — Multi-Device Sync (Planned)

**Status:** Backlog  
**Target:** 2026-09

### Overview
Sync SQLite data across user devices with conflict resolution.

### Key Decisions Pending
- Sync mechanism (file-based via cloud storage vs self-hosted service)
- Conflict resolution strategy (last-write-wins vs semantic merge)
- Rollback and consistency guarantees

---

## Dependencies & Critical Path

```
Phase 1–9 (Foundation)
  ↓
Phase 10 (Memory & KMS)
  ↓
Phase 11 (Self-Improvement) ← Complete
  ├→ Phase 12 (Agentic Optimization) — uses skill confidence for routing
  └→ Phase 13 (Voice & Multimodal) — independent track
      ↓
Phase 14 (Multi-Device Sync) — depends on stable schema (Phase 5+)
```

---

## Success Metrics

### Phase 11 (Shipped)
- [x] Skill synthesis runs hourly with no failures
- [x] EMA confidence reflects actual success rates
- [x] Decay worker archives stale skills (tested on synthetic data)
- [x] Injection screening blocks all BLOCKED_PHRASES
- [x] Feedback detection handles Vietnamese + English patterns
- [x] Per-turn traces recorded for all agent runs

### Phase 12 (In Progress)
- [ ] Router selects optimal model tier (fast/medium/thinking/ultra) per task
- [ ] Cost savings measured per deployment cohort
- [ ] Sub-agent spawning has <50ms overhead
- [ ] Sub-agent lifecycle cleanup runs without resource leaks

---

## Known Blockers

None currently. All Phase 11 deliverables shipped.

---

## Maintenance & Tech Debt

### Phase 11 (Current)
- **Jaccard threshold (0.40)**: Tuned on synthetic data; validate on real traces
- **Decay lambda (0.693/24h)**: Assumes hourly worker; adjust if schedule changes
- **Archive threshold (0.30)**: Conservative; monitor for false positives
- **BLOCKED_PHRASES**: Expand as injection patterns emerge

### Recommended Review
- EMA alpha (0.10) sensitivity analysis at month 3 of Phase 11
- Skill database bloat: monitor for slow queries on traces table (1M+ rows)
- Skill injection attempts: track and update screening rules quarterly
