# Haily — Architecture Decisions

Ghi lại các quyết định kỹ thuật. Mỗi quyết định có lý do và trade-off.

---

## Tổng quan

```
haily (single Rust binary)
│
├── Mode: --gui        → Tauri + Svelte UI
├── Mode: --cli        → terminal REPL
└── Mode: --headless   → background daemon (VPS, cloud)

Core
├── Intelligence
│   ├── Local inference  (ollama API → candle fallback)
│   └── LLM API client   (Anthropic, OpenAI, Gemini...)
├── Memory               (SQLite, local per-device)
├── Agent orchestrator   (Haily Core + sub-agents)
├── Telegram client      (background, optional push channel)
└── Sync module          (placeholder — future)
```

---

## Quyết định 1 — Ngôn ngữ: Rust

**Quyết định:** Toàn bộ codebase viết bằng Rust.

**Lý do:**
- Compile to native trên Win/Mac/Linux/Android/iOS
- Performance phù hợp cho local AI inference và always-on daemon
- Single binary — không có runtime dependency (khác Go, Python, Node)
- Memory safety quan trọng với một app chạy liên tục access dữ liệu cá nhân

**Trade-off:** Ecosystem Rust cho AI/LLM chưa mature như Python. Giải quyết bằng cách dùng ollama làm inference layer (ollama viết Go, expose HTTP API).

---

## Quyết định 2 — UI Framework: Tauri + Svelte 5 + shadcn-svelte

**Quyết định:** Tauri làm shell native, Svelte 5 làm frontend, shadcn-svelte làm component library, Shiki làm syntax highlighter.

**Lý do chọn Tauri thay Slint:**
- Chat AI cần render markdown, code block với syntax highlight — Slint không có sẵn, phải tự build renderer tốn effort không liên quan core product
- Tauri 2.0 mobile (Android/iOS) mature hơn Slint experimental
- Dùng WebView hệ điều hành (không bundle Chromium như Electron) → overhead chấp nhận được

**Memory overhead thực tế:**
- WebView thêm ~50–80MB RAM
- Local LLM model dùng 4–8GB RAM
- → WebView overhead < 2% tổng, không đáng kể

**Lý do chọn Svelte 5 thay React:**
- Bundle nhỏ hơn (~20–40KB vs ~150KB+)
- Compile-time, không có virtual DOM runtime trong WebView
- Giảm overhead WebView thêm một bậc

**Full frontend stack:**
- Tauri 2.0 (shell + native APIs)
- Svelte 5 (framework)
- shadcn-svelte (component library)
- Tailwind CSS v4
- Shiki (syntax highlighting — cùng engine VS Code/Cursor)
- marked + @tailwindcss/typography (markdown rendering)

**Trade-off — Mobile support:**

| Platform | Tauri 2.0 status | Hành động |
|----------|-----------------|-----------|
| Windows | Production | Ship |
| macOS | Production | Ship |
| Linux | Production | Ship |
| Android | Functional nhưng còn giới hạn¹ | Defer — quyết định sau khi Tauri 3.x ổn định hơn |
| iOS | Functional nhưng còn giới hạn¹ | Defer — quyết định sau khi Tauri 3.x ổn định hơn |

¹ Verified qua research (2026-06): multi-webview API không có trên mobile, localhost HTTP bị block trên Android (phải dùng `asset://`), clipboard plugin lỗi sau APK packaging. Mobile ecosystem của Tauri trẻ hơn desktop ~2 năm.

**Mobile strategy trong thời gian chờ:**
Haily đã có Telegram integration qua `haily-io` Adapter. User dùng điện thoại → nhắn Telegram bot → Haily xử lý trên PC/VPS → reply qua Telegram. Không cần mobile app. Đây là experience đủ tốt cho headless/remote use case mà không cần ship mobile app riêng.

Khi nào làm mobile app: Tauri 3.x stable (ước tính 2027+) hoặc Flutter/React Native + Rust core FFI nếu cần sớm hơn.

**Không chọn Flutter:** Yêu cầu Dart ecosystem riêng, không phù hợp Rust-first.

---

## Quyết định 3 — Kiến trúc: Single Binary (không phải client/server)

**Quyết định:** Haily là một process duy nhất, không tách server/client.

**Lý do:**
- Đơn giản hơn: không cần quản lý IPC, socket, API contract giữa client và server
- Offline-first: toàn bộ intelligence chạy trong cùng process với UI
- Zero setup: user cài xong là dùng, không cần start server riêng

**Trade-off — Sync giữa devices:** Khi user dùng cả PC lẫn điện thoại, data không tự đồng bộ. Quyết định: defer — sẽ giải quyết sau khi có nhu cầu rõ ràng. Thiết kế SQLite schema sync-friendly từ đầu (timestamp-based, conflict-free) để không bị lock-in.

**Deployment modes (cùng binary):**
- `haily --gui` → Desktop app với Tauri UI
- `haily --cli` → Terminal REPL, phù hợp SSH/scripting
- `haily --headless` → Daemon không UI, chạy trên VPS; user tương tác qua các channels (Telegram, Email, Discord, v.v.)

---

## Quyết định 4 — AI Inference: Ollama first, Candle fallback

**Quyết định:** Local AI chạy qua ollama HTTP API. Candle (native Rust) làm embedded fallback.

**Tại sao không llama.cpp bindings trực tiếp:** Build cực kỳ phức tạp (C++ FFI, CUDA/Metal/Vulkan backends), tăng build time đáng kể, khó maintain.

**Tại sao ollama first:**
- User tự quản lý model (pull/delete) qua ollama CLI
- Haily chỉ cần gọi HTTP `localhost:11434`
- Ollama đã handle GPU acceleration, quantization, model loading
- Nếu user không có ollama → tự động fallback về cloud API

**Quyết định:** llama.cpp embedded (qua `llama-cpp-2` Rust crate) làm primary local inference. Ollama là optional enhancement cho power users.

**Tại sao llama.cpp embedded thay vì yêu cầu cài Ollama:**
- "Cài xong là dùng" — user không cần biết Ollama tồn tại
- Single binary thực sự: model download lần đầu, chạy offline mãi sau
- `llama-cpp-2` crate cung cấp pre-built CPU binaries — CPU-only build không cần CUDA/Metal
- GPU acceleration là optional feature flag, không block default build
- Reference codebase Go cũ đã dùng approach này thành công

**Offline model mặc định: Qwen2.5:3b (GGUF Q4_K_M)**
- Hỗ trợ 29 ngôn ngữ, tiếng Việt verified tốt
- ~1.9 GB RAM, ~12–15 tok/s trên CPU laptop hiện đại
- Low-RAM option: `qwen2.5:1.5b` (~1.0 GB)
- Advanced option cho user có GPU: Vistral-7B, SeaLLM-7B (tiếng Việt tốt hơn)

**LLM routing:**
```
Request
  → llama.cpp embedded (luôn available)  ← primary offline, mặc định
  → Ollama detected?    YES → dùng Ollama ← power user override, GPU support tốt hơn
  → online?             YES → cloud API (Anthropic / OpenAI / Gemini)
```

User có thể force cloud API hoặc force local qua config.

---

## Quyết định 5 — Storage: SQLite local

**Quyết định:** Mọi data lưu SQLite trên thiết bị. Không có remote database bắt buộc.

**Lý do:** Offline-first. Data thuộc về user, chạy không cần internet.

**Schema constraints (để sync-friendly sau này):**
- Mọi record có `created_at`, `updated_at` (RFC3339 UTC)
- Soft delete: `deleted_at` thay vì xóa thật
- UUID làm primary key (không dùng auto-increment integer — conflict khi merge)
- Không có foreign key cascade delete (safe để merge từ nhiều sources)

**Sync options khi cần (chưa quyết định):**
1. File sync qua cloud storage (iCloud/GDrive/Dropbox) — zero server
2. Telegram file transfer — export SQLite → gửi → import
3. Self-hosted sync service nhỏ

---

## Quyết định 6 — Communication: Telegram

**Quyết định:** Telegram là push channel chính khi Haily chạy headless.

**Lý do:**
- Bot API đầy đủ, không cần đăng ký business
- Haily → User: proactive alerts, morning brief, reminders
- User → Haily: gửi lệnh từ xa qua chat

**Zalo:** Defer. Zalo OA yêu cầu đăng ký doanh nghiệp Việt Nam và hạn chế nhiều tính năng. Không block nhưng không ưu tiên.

---

---

## Quyết định 7 — Vector search và Embeddings

**Quyết định:** `hnsw_rs` làm HNSW index (in-memory, rebuilt at startup). `fastembed-rs` với model `multilingual-e5-base` để generate embeddings.

**Tại sao hnsw_rs thay instant-distance:**
- `instant-distance` last update 2023-06, inactive. `hnsw_rs` last update 2026-02, maintained bởi tác giả bigann benchmark
- hnsw_rs hỗ trợ multithreaded insert, memory-mapping persistence (`dump_hnsw`/`load_hnsw`), filterable search
- Cùng scale 50K vectors, hnsw_rs cho throughput cao hơn nhờ rayon parallelism

**Embedding dims: 768 (không phải 1536)**
- Model: `multilingual-e5-base` (768 dims, 278M params, Vietnamese-capable)
- 50K × 768 dims × 4 bytes ≈ 150 MB RAM — chấp nhận được
- `multilingual-e5-large` (1024 dims) nếu cần chất lượng cao hơn

**Tại sao fastembed-rs thay Ollama embeddings:**
- fastembed-rs: embedded trong process, không cần Ollama running, offline reliable
- ONNX runtime backend — đủ nhanh cho embedding generation
- Ollama embeddings chất lượng MTEB tốt hơn nhưng adds HTTP round-trip latency và dependency

---

## Quyết định 8 — Self-Improvement Loop: Skill Synthesis & Decay

**Quyết định:** Haily tự động học từ task traces, tổng quát hóa thành reusable skills, track confidence via EMA, và decay skills cũ kém chất lượng.

**Kiến trúc:**

```
User interaction
  ↓
Agent completes task
  ├── Record task trace (description, tool calls, outcome, duration)
  └── Detect user feedback (👍/👎/corrections)
      ↓
      Apply feedback signal → update preferences & model context
      ↓
      Every 1 hour: Synthesis worker runs
      │ ├── Load recent traces (1h window)
      │ ├── Cluster by Jaccard similarity (threshold 0.40)
      │ ├── Ask LLM to generalize each cluster → skill
      │ ├── Screen for injection (forbidden phrases, control chars)
      │ └── Save new skill (confidence = 1.0)
      ↓
      Every 24 hours: Decay worker runs
      ├── Apply exponential decay: conf *= e^(-λ*t) where λ=0.693/24h
      └── Archive skills where conf < 0.30
```

**Tại sao Jaccard clustering thay embedding:**
- Jaccard (word set overlap) không cần embedding model — tiết kiệm 1 LLM inference
- Simple, deterministic, easy to tune (threshold 0.40)
- Đủ tốt cho task trace generalization (150–300 từ per trace)

**Tại sao EMA confidence (α=0.10):**
- α=0.10 = khoảng 10 events để confidence stabilize
- On success: new_conf = 0.10 * 1.0 + 0.90 * old_conf
- On failure: new_conf = 0.10 * 0.0 + 0.90 * old_conf
- Thấp enough để respond quickly, cao enough để avoid noise

**Tại sao exponential decay (λ=0.693/24h):**
- Half-life = 24 hours (confidence drops to 50% after 1 day no use)
- λ = ln(2) / 24h ≈ 0.0289 per hour
- Archive threshold = 0.30 (skills rarely used in 2–3 days disappear)
- Giữ library compact, avoid serving stale patterns

**Feedback signals:**
```rust
enum FeedbackSignal {
    Positive,           // 👍, tốt, hay, perfect, thank, good
    Negative { topic }, // 👎, sai, dài quá, wrong, bad (with optional topic)
    Correction { old, new }, // "không phải X mà là Y"
}
```
- Detect from user message text (Vietnamese + English patterns)
- Store as preferences (positive_streak, prefer_shorter_responses, corrections)
- Inform next LLM context (prefer_shorter_responses → constraint on response_tokens)

**Injection screening:**
- Blocks phrases: "ignore instructions", "system:", "eval(", "exec(", etc.
- Strips excessive control characters (> 5 = reject)
- JSON parsing strict: reject if not valid JSON

**Task trace schema:**
```
id (UUID)
session_id (link to session)
task_description (natural language, ~150 words)
tool_calls (JSON array)
outcome ("success", "failure", "partial")
duration_ms (ms to complete)
created_at (RFC3339 UTC)
```

**Worker lifecycle:**
- `Orchestrator::init()` spawns both workers as background tasks
- Synthesis worker: runs hourly, silently continues on LLM error (logs warning)
- Decay worker: runs daily (24h after previous run)
- Both workers are idempotent (safe to re-run, no duplicate skills)

**Related modules:**
- `haily_kms::skills` — synthesis, EMA update, decay
- `haily_kms::feedback` — FeedbackSignal enum, apply_feedback_signal()
- `haily_core::feedback_parser` — detect_feedback(msg)
- `haily_db::queries::skills` — insert_trace(), insert_skill(), update_skill_confidence(), apply_exponential_decay()

---

## Điều chưa quyết định

- Sync mechanism khi user có nhu cầu multi-device
- Voice STT trên desktop (Whisper local vs cloud API)
- Permission/security model cho local AI (prompt injection, data boundary)
- Update mechanism cho binary (self-update hay manual)
- ~~Mobile strategy~~ → **Đã quyết định:** Telegram làm mobile interface tạm thời. Native mobile app defer đến Tauri 3.x stable hoặc Flutter + Rust FFI nếu cần sớm hơn.
- ~~Skill synthesis~~ → **Đã quyết định (Phase 11):** Jaccard clustering + LLM generalize + hourly/daily workers. Confidence tracking via EMA (α=0.10). Exponential decay (λ=0.693/24h, archive < 0.30).
