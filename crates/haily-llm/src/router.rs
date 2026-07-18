use crate::CloudClient;
#[cfg(feature = "llama")]
use crate::prompt::PromptFormat;
use crate::{CompletionRequest, LlmClient, StreamChunk};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Placeholder used when no backend is configured.
/// Returns a user-visible error on every completion so the app still starts.
struct NoopClient;

#[async_trait]
impl LlmClient for NoopClient {
    async fn complete(&self, _req: CompletionRequest) -> Result<String> {
        Err(anyhow::anyhow!(
            "Chưa cấu hình LLM. Mở Settings → Model LLM để chọn model."
        ))
    }

    async fn complete_stream(&self, _req: CompletionRequest) -> Result<mpsc::Receiver<StreamChunk>> {
        Err(anyhow::anyhow!(
            "Chưa cấu hình LLM. Mở Settings → Model LLM để chọn model."
        ))
    }

    fn provider_name(&self) -> &str { "unconfigured" }
    fn context_window(&self) -> u32 {
        // Never actually budgeted against — every `complete()` call errors first.
        // A conservative non-zero value avoids a divide-by-zero-shaped footgun if a
        // caller ever budgets before checking `provider_name()`.
        DEFAULT_LLAMA_N_CTX
    }
}

/// Per-provider context-window constants used for budgeting (`context_window()`).
/// Clamped, not the provider's true native maximum, so history sizing stays sane
/// immediately after a hot-swap between backends with very different windows.
pub(crate) const DEFAULT_LLAMA_N_CTX: u32 = 8192;
/// Cloud providers advertise context windows far larger than a local model
/// (e.g. 128k-200k) — clamped to 32k for budgeting per phase-05 spec so a session's
/// history doesn't balloon to an unreasonable size just because the backend changed.
pub(crate) const CLOUD_CONTEXT_WINDOW_CLAMP: u32 = 32_000;

/// Default `LlmConfig::cost_quality` — see that field's doc for the rationale.
const DEFAULT_COST_QUALITY: u8 = 7;
/// Upper bound `LlmRouter::cost_quality()` clamps to (dial is 0-10 inclusive).
const COST_QUALITY_MAX: u8 = 10;

#[cfg(feature = "llama")]
use crate::LlamaClient;

/// Model-tier foundation (Phase 7 — wired but inert): names a routing tier a
/// `DomainConfig`/`SpecialistConfig` can request. Full complexity-based
/// auto-routing is deliberately NOT implemented (YAGNI until a task-outcome
/// quality signal exists) — today a tier only changes which *cloud model name*
/// a completion uses, never the backend/provider itself.
///
/// ORDINAL ORDERING (Phase 3): variants are declared low→high (`Fast < Medium <
/// Thinking < Ultra`) so the derived `PartialOrd`/`Ord` matches the HailyKit
/// model-map vocabulary 1:1 — an escalation `T→T+1` is just `Tier::next()`, and a
/// `max_tier` cap is a `<=` comparison. Do NOT reorder these variants: the derive
/// keys off declaration order, so a reorder silently inverts every tier comparison.
///
/// `Ultra` is *cloud-effective-only* (DEP-1): it names one more cloud-model-name
/// override, never a new backend. ollama-style local backends map `Thinking`+`Ultra`
/// to the same loaded GGUF, so a local-only escalation to `Ultra` short-circuits to a
/// no-op — handled by the egress cap in [`crate::escalation`], not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tier {
    Fast,
    Medium,
    Thinking,
    Ultra,
}

impl Tier {
    /// The next-higher tier, or `None` at the ceiling (`Ultra`). Used by
    /// [`crate::escalation::EscalationPolicy`] to compute a `T→T+1` step; the policy
    /// applies the `max_tier`/egress cap on top of this raw successor.
    pub fn next(self) -> Option<Tier> {
        match self {
            Tier::Fast => Some(Tier::Medium),
            Tier::Medium => Some(Tier::Thinking),
            Tier::Thinking => Some(Tier::Ultra),
            Tier::Ultra => None,
        }
    }
}

/// Curated `model_name → Tier` map — an OFFLINE-derived subset of the HailyKit
/// model-map, informed by AutomationBench-AA leaderboard scores (a published proxy
/// for multi-step multi-tool business-automation quality; Phase 3 spec §External
/// signal). There is intentionally NO integration code that reads AA at runtime — the
/// scores inform which tier each model lands in here, nothing more.
///
/// Resolution is EXACT-match-first then substring (see [`resolve_model_tier`]) so a
/// query for `"gpt-5.4"` never fuzzy-hits the `"gpt-5.4-mini"` entry — the substring-bug
/// fix inherited from the depth-tier plan. An unknown model resolves to `None`
/// (fail-safe: downstream scaffolds stay ON, tier hints OFF).
const CURATED_MODEL_TIERS: &[(&str, Tier)] = &[
    // Fast — small/cheap, single-step reliable.
    ("gpt-4o-mini", Tier::Fast),
    ("gpt-5.4-mini", Tier::Fast),
    ("claude-3-5-haiku", Tier::Fast),
    ("gemini-1.5-flash", Tier::Fast),
    // Medium — general-purpose workhorse.
    ("gpt-4o", Tier::Medium),
    ("claude-3-5-sonnet", Tier::Medium),
    ("gemini-1.5-pro", Tier::Medium),
    // Thinking — strong multi-step reasoning.
    ("gpt-5.4", Tier::Thinking),
    ("claude-3-7-sonnet", Tier::Thinking),
    ("o1", Tier::Thinking),
    // Ultra — top-tier automation (highest AA band).
    ("claude-opus-4", Tier::Ultra),
    ("o1-pro", Tier::Ultra),
    ("gpt-5.4-pro", Tier::Ultra),
];

/// Resolve a model name to its routing [`Tier`], preferring caller `overrides` over the
/// built-in [`CURATED_MODEL_TIERS`], and EXACT match over substring in BOTH.
///
/// Two-pass by design (Phase 3 spec step 3): pass 1 requires string equality, so
/// `"gpt-5.4"` binds to its own `Thinking` entry rather than substring-matching the
/// longer `"gpt-5.4-mini"` (`Fast`). Only if no exact key exists does pass 2 look for a
/// curated/override key that is a substring of `model_name` (e.g. a dated deployment id
/// `"gpt-4o-mini-2024-07-18"` matching `"gpt-4o-mini"`); the LONGEST such key wins so a
/// more specific entry beats a shorter prefix. **Overrides win over the curated table in
/// BOTH passes** — pass 2 checks overrides first, so an operator's override is honored even
/// when a longer curated key would otherwise substring-match. Unknown model → `None` (fail-safe).
pub fn resolve_model_tier(model_name: &str, overrides: &[(String, Tier)]) -> Option<Tier> {
    // Pass 1: exact match. Overrides win over the curated table.
    if let Some((_, t)) = overrides.iter().find(|(k, _)| k == model_name) {
        return Some(*t);
    }
    if let Some((_, t)) = CURATED_MODEL_TIERS.iter().find(|(k, _)| *k == model_name) {
        return Some(*t);
    }
    // Pass 2: substring, OVERRIDES FIRST — any override substring match beats the curated
    // table (so "overrides win" holds for substrings too, not just exact keys); within each
    // source the LONGEST matching key wins.
    longest_substring_match(model_name, overrides.iter().map(|(k, t)| (k.as_str(), *t)))
        .or_else(|| {
            longest_substring_match(model_name, CURATED_MODEL_TIERS.iter().map(|(k, t)| (*k, *t)))
        })
}

/// The tier of the LONGEST `table` key that is a substring of `model_name` (empty keys
/// ignored); `None` if none match.
fn longest_substring_match<'a>(
    model_name: &str,
    table: impl Iterator<Item = (&'a str, Tier)>,
) -> Option<Tier> {
    let mut best: Option<(usize, Tier)> = None;
    for (key, tier) in table {
        if !key.is_empty() && model_name.contains(key) && best.is_none_or(|(len, _)| key.len() > len)
        {
            best = Some((key.len(), tier));
        }
    }
    best.map(|(_, t)| t)
}

/// Immutable per-run view of tier→model resolution, captured once at run start.
///
/// RED-TEAM FMA-m3 (Phase 3): a live `reload_llm` between escalation attempts would
/// change tier→model resolution mid-run, corrupting escalation counting and eval
/// reproducibility. The P4 pipeline runner snapshots this once per run and consults it
/// for every stage/attempt; a config reload swaps the whole `Arc<LlmRouter>` and thus
/// only takes effect at the NEXT run boundary. Consumed by P4 — not wired into any live
/// loop this phase.
#[derive(Debug, Clone)]
pub struct RouterSnapshot {
    default_model: String,
    tier_models: HashMap<Tier, String>,
}

impl RouterSnapshot {
    /// The cloud model name a call at `tier` would use: the tier's configured override
    /// if present, else the session default model (mirrors `complete_tiered`'s fallback).
    pub fn model_for_tier(&self, tier: Option<Tier>) -> &str {
        tier.and_then(|t| self.tier_models.get(&t))
            .map(String::as_str)
            .unwrap_or(&self.default_model)
    }

    /// The session's effective tier, derived from its default model via
    /// [`resolve_model_tier`]. `None` when the default model is not in the curated
    /// table or `overrides` (fail-safe — feeds P7's "scaffold when tier < ultra" hint).
    pub fn session_tier(&self, overrides: &[(String, Tier)]) -> Option<Tier> {
        resolve_model_tier(&self.default_model, overrides)
    }
}

/// One tier's cloud endpoint override: which model to call, and optionally its own
/// base URL / API-key pool.
///
/// `base_url == None` inherits [`LlmConfig::cloud_base_url`]; `api_keys == None` (or an
/// empty vec) inherits [`LlmConfig::cloud_api_keys`]. This is the hybrid model-config
/// contract: a single-provider / aggregator user (e.g. OpenRouter) leaves both `None` and
/// only names a model per tier, while a direct multi-provider user overrides the endpoint
/// and keys per tier. Key material here follows the same no-log discipline as
/// `LlmConfig::cloud_api_keys` — never emit it to `tracing`.
#[derive(Debug, Clone)]
pub struct TierEndpoint {
    pub model: String,
    pub base_url: Option<String>,
    pub api_keys: Option<Vec<String>>,
}

impl TierEndpoint {
    /// A tier endpoint that names only a model and inherits the session-default base URL
    /// and API-key pool (the aggregator / single-provider case).
    pub fn inherit(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: None,
            api_keys: None,
        }
    }
}

/// Optional cloud endpoint override per tier. Every field defaults to `None`,
/// meaning "no override — use `LlmConfig::cloud_model` on the default endpoint" — this is
/// the zero-behavior-change default the phase-07 spec requires.
#[derive(Debug, Clone, Default)]
pub struct TierModels {
    pub fast: Option<TierEndpoint>,
    pub medium: Option<TierEndpoint>,
    pub thinking: Option<TierEndpoint>,
    pub ultra: Option<TierEndpoint>,
}

impl TierModels {
    fn get(&self, tier: Tier) -> Option<&TierEndpoint> {
        match tier {
            Tier::Fast => self.fast.as_ref(),
            Tier::Medium => self.medium.as_ref(),
            Tier::Thinking => self.thinking.as_ref(),
            Tier::Ultra => self.ultra.as_ref(),
        }
    }
}

/// Runtime configuration for LLM routing.
/// Loaded from KMS preferences on startup; user can update without restart.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// One or more API keys for the cloud backend.
    /// Multiple keys rotate round-robin; empty = no cloud backend.
    pub cloud_api_keys: Vec<String>,
    pub cloud_base_url: String,
    pub cloud_model: String,
    /// Per-tier cloud model-name overrides — see `Tier`/`TierModels`. All `None`
    /// by default (no behavior change until a caller opts a domain/specialist in).
    pub tier_models: TierModels,
    /// Operator's cost/quality dial (0 = cheapest, 10 = highest-quality), consulted by
    /// `EscalationPolicy` (Phase 4/6) when deciding whether an escalation attempt is
    /// worth its cost. Defaults to 7 (mildly quality-biased) so existing deployments
    /// that never set this preference get a reasonable default rather than 0. Stored
    /// unclamped here — [`LlmRouter::cost_quality`] clamps at the point of use so an
    /// out-of-range preference value still round-trips losslessly through storage.
    pub cost_quality: u8,

    /// Path to GGUF model file for embedded inference (only used with `llama` feature).
    #[cfg(feature = "llama")]
    pub llama_model_path: Option<std::path::PathBuf>,
    /// Context window size for llama.cpp (default 8192 — ~295MB KV cache for
    /// Qwen2.5-3B, acceptable on any laptop capable of running a 3B model; see
    /// research report 03 §A1). User-configurable via the `llm.llama_n_ctx` preference.
    #[cfg(feature = "llama")]
    pub llama_n_ctx: u32,
    /// Prompt format used when formatting messages for the GGUF model.
    /// ChatML for Qwen2.5; Gemma4 for google/gemma-4 GGUF files.
    #[cfg(feature = "llama")]
    pub llama_prompt_format: PromptFormat,
    /// Number of model layers to offload to GPU.
    /// 0 = CPU-only; 999 = full GPU offload (llama.cpp clamps to actual layer count).
    /// Auto-detected from compiled GPU features; override via `llm.llama_n_gpu_layers` preference.
    #[cfg(feature = "llama")]
    pub llama_n_gpu_layers: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        // Collect env-var keys available at startup (may be overridden by DB prefs).
        let env_keys: Vec<String> = ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "HAILY_CLOUD_KEY"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .collect();
        Self {
            cloud_api_keys: env_keys,
            cloud_base_url: "https://api.openai.com".into(),
            cloud_model: "gpt-4o-mini".into(),
            tier_models: TierModels::default(),
            cost_quality: DEFAULT_COST_QUALITY,
            #[cfg(feature = "llama")]
            llama_model_path: None,
            #[cfg(feature = "llama")]
            llama_n_ctx: DEFAULT_LLAMA_N_CTX,
            #[cfg(feature = "llama")]
            llama_prompt_format: PromptFormat::ChatML,
            #[cfg(feature = "llama")]
            llama_n_gpu_layers: crate::gpu::default_gpu_layers(),
        }
    }
}

/// Builds one `CloudClient` per tier that names a model override in
/// `config.tier_models`. Each tier's `base_url` / `api_keys` come from its own
/// [`TierEndpoint`] when set, else inherit `config`'s session defaults (the hybrid
/// contract). Silently skips (with a warning) a tier whose resolved key pool is empty —
/// both its own AND the inherited default are empty — since `complete_tiered` then
/// degrades to `primary`, which is always correct.
///
/// Note: unlike the pre-hybrid version, there is NO early return when the session-default
/// keys are empty — a tier may carry its OWN keys (direct-provider case) even when no
/// default pool is configured.
/// The effective API-key pool for a tier endpoint: its OWN keys when non-empty, else the
/// session-default pool. An empty result means no keys resolve anywhere — the tier cannot
/// build a client and routing falls back to `primary`. Shared by [`build_tier_clients`] and
/// [`tier_model_names`] so the snapshot never names a tier the router can't actually route to.
fn resolve_tier_keys(config: &LlmConfig, ep: &TierEndpoint) -> Vec<String> {
    match &ep.api_keys {
        Some(k) if !k.is_empty() => k.clone(),
        _ => config.cloud_api_keys.clone(),
    }
}

fn build_tier_clients(config: &LlmConfig) -> HashMap<Tier, Arc<dyn LlmClient>> {
    let mut clients: HashMap<Tier, Arc<dyn LlmClient>> = HashMap::new();
    for tier in [Tier::Fast, Tier::Medium, Tier::Thinking, Tier::Ultra] {
        let Some(ep) = config.tier_models.get(tier) else {
            continue;
        };
        // Inherit-or-override: a blank per-tier base_url/keys falls back to the session
        // defaults, so an aggregator user who only names models gets the default endpoint.
        let base_url = ep
            .base_url
            .clone()
            .unwrap_or_else(|| config.cloud_base_url.clone());
        let keys = resolve_tier_keys(config, ep);
        if keys.is_empty() {
            tracing::warn!(
                ?tier,
                "tier model override set but no API keys (own or inherited) — skipping; routing falls back to primary"
            );
            continue;
        }
        match CloudClient::new(base_url, keys, &ep.model) {
            Ok(client) => {
                clients.insert(tier, Arc::new(client));
            }
            Err(e) => tracing::warn!(?tier, "failed to build tier cloud client: {e:#}"),
        }
    }
    clients
}

/// Effective cloud model name for each tier that names an override in `config`.
/// Captured at `init` into [`LlmRouter`] so [`RouterSnapshot`] can report what each
/// tier resolves to without holding a live `LlmConfig`. Tiers with no override are
/// absent (the snapshot falls back to the default model, mirroring `complete_tiered`).
fn tier_model_names(config: &LlmConfig) -> HashMap<Tier, String> {
    let mut names = HashMap::new();
    for tier in [Tier::Fast, Tier::Medium, Tier::Thinking, Tier::Ultra] {
        if let Some(ep) = config.tier_models.get(tier) {
            // Only report a tier the router can actually build a client for — a tier whose
            // keys resolve empty (own blank AND default blank) is skipped by
            // `build_tier_clients`, so naming it here would make the badge/snapshot report
            // a model routing never uses (it silently falls back to `primary`).
            if !resolve_tier_keys(config, ep).is_empty() {
                names.insert(tier, ep.model.clone());
            }
        }
    }
    names
}

/// Routes requests to the best available LLM backend.
///
/// Priority:
///   1. llama.cpp embedded  (feature = "llama", model file present)
///   2. Cloud API           (api_key configured)
pub struct LlmRouter {
    primary: Arc<dyn LlmClient>,
    fallback: Option<Arc<dyn LlmClient>>,
    /// One cloud client per tier that has a configured model-name override.
    /// An absent entry means "no override" — `complete_tiered` falls back to
    /// `primary` in that case. Built once at `init`/`reload_llm` time so a
    /// hot-swap picks up new tier overrides exactly like the primary does.
    tier_clients: HashMap<Tier, Arc<dyn LlmClient>>,
    /// Default (session) model name — `LlmConfig::cloud_model` at init time. Feeds
    /// [`RouterSnapshot`] only; routing itself never consults it (that goes through
    /// `primary`/`tier_clients`).
    default_model: String,
    /// Effective model name per tier that has an override, captured at init. Feeds
    /// [`RouterSnapshot::model_for_tier`] — see [`tier_model_names`].
    tier_model_names: HashMap<Tier, String>,
    /// Clamped copy of `LlmConfig::cost_quality`, captured at init/reload so
    /// `cost_quality()` never has to re-clamp an already-moved `LlmConfig`.
    cost_quality: u8,
}

impl LlmRouter {
    /// Always succeeds — uses `NoopClient` when no backend is reachable so the
    /// app can start without a configured model. The error surfaces only when the
    /// user actually sends a message.
    pub async fn init(config: LlmConfig) -> Self {
        let cloud: Option<Arc<dyn LlmClient>> = if !config.cloud_api_keys.is_empty() {
            match CloudClient::new(
                config.cloud_base_url.clone(),
                config.cloud_api_keys.clone(),
                config.cloud_model.clone(),
            ) {
                Ok(client) => Some(Arc::new(client) as Arc<dyn LlmClient>),
                Err(e) => {
                    tracing::warn!("failed to build cloud HTTP client: {e:#}");
                    None
                }
            }
        } else {
            None
        };

        // Tier clients only make sense against the cloud backend (llama.cpp has a
        // single loaded GGUF file — there is no "different model" to route a tier
        // to locally). Built from the same base_url/keys as `cloud`, one extra
        // `CloudClient` per tier that names an override.
        let tier_clients = build_tier_clients(&config);
        // Snapshot inputs captured up front: the llama branch below moves
        // `config.llama_n_ctx` into a `spawn_blocking` closure (disjoint capture keeps
        // the rest of `config` usable), so compute these before that point to keep the
        // read sites unambiguous across feature flags.
        let default_model = config.cloud_model.clone();
        let tier_model_names = tier_model_names(&config);
        let cost_quality = config.cost_quality.min(COST_QUALITY_MAX);

        #[cfg(feature = "llama")]
        {
            // `filter(|p| p.exists())` keeps the path and existence check tied together —
            // no separate boolean flag that could drift from the `Option` it was derived
            // from, and no `.unwrap()` needed to re-extract the path afterward.
            let existing_model_path = config
                .llama_model_path
                .clone()
                .filter(|p| p.exists());

            if let Some(path) = existing_model_path {
                let fmt = config.llama_prompt_format;
                let n_gpu_layers = config.llama_n_gpu_layers;
                tracing::info!(
                    "LLM: llama.cpp ({}) — {}",
                    path.display(),
                    crate::gpu::gpu_mode_label(n_gpu_layers)
                );
                match tokio::task::spawn_blocking(move || {
                    LlamaClient::load(path, config.llama_n_ctx, fmt, n_gpu_layers)
                }).await {
                    Ok(Ok(client)) => return Self {
                        primary: Arc::new(client),
                        fallback: cloud,
                        tier_clients,
                        default_model,
                        tier_model_names,
                        cost_quality,
                    },
                    Ok(Err(e)) => tracing::warn!("llama.cpp load failed: {e:#}"),
                    Err(e)     => tracing::warn!("llama.cpp spawn failed: {e:#}"),
                }
            }
        }

        if let Some(cloud_client) = cloud {
            tracing::info!("LLM: cloud API ({})", config.cloud_model);
            return Self {
                primary: cloud_client,
                fallback: None,
                tier_clients,
                default_model,
                tier_model_names,
                cost_quality,
            };
        }

        tracing::warn!("No LLM backend configured — open Settings → Model LLM");
        Self {
            primary: Arc::new(NoopClient),
            fallback: None,
            tier_clients,
            default_model,
            tier_model_names,
            cost_quality,
        }
    }

    /// Capture an immutable [`RouterSnapshot`] of this router's tier→model resolution.
    /// The P4 pipeline runner calls this once per run so a live `reload_llm` (which
    /// swaps the whole `Arc<LlmRouter>`) cannot change resolution mid-run — see
    /// [`RouterSnapshot`]'s doc for the reproducibility contract.
    pub fn snapshot(&self) -> RouterSnapshot {
        RouterSnapshot {
            default_model: self.default_model.clone(),
            tier_models: self.tier_model_names.clone(),
        }
    }

    /// Complete a request against a specific model `tier`. Falls back to `primary`
    /// (the default model) when `tier` is `None`, or when `Some(tier)` names a tier
    /// with no configured override — this is the "wired but inert" contract: callers
    /// may pass a tier freely and always get a working completion, with zero
    /// behavior change until an operator actually configures `LlmConfig::tier_models`.
    pub async fn complete_tiered(&self, tier: Option<Tier>, req: CompletionRequest) -> Result<String> {
        match tier.and_then(|t| self.tier_clients.get(&t)) {
            Some(client) => client.complete(req).await,
            None => self.complete(req).await,
        }
    }

    pub fn provider_name(&self) -> &str {
        self.primary.provider_name()
    }

    /// Whether the `Ultra` tier can reach a genuinely cloud-backed model. `Ultra` is
    /// cloud-effective-only (see [`Tier`]): a local-only backend maps `Thinking`+`Ultra`
    /// to the one loaded GGUF, so an `Ultra` request there silently collapses to the
    /// session model. The phase-7 apex-judge/synthesis calls consult this to decide
    /// whether to warn + fall back to the session tier instead of pretending they
    /// escalated. `true` when a dedicated Ultra override client exists, the primary is the
    /// cloud backend, or a cloud fallback is configured; `false` for a local-only setup.
    pub fn ultra_reachable(&self) -> bool {
        self.tier_clients.contains_key(&Tier::Ultra)
            || self.primary.provider_name() == "cloud"
            || self.fallback.is_some()
    }

    /// Context window (tokens) of the currently-active backend, for `haily-core`'s
    /// token budgeter. Reflects `primary`, not `fallback` — a request that spills
    /// over to the cloud fallback mid-flight is rare enough (and the fallback's
    /// window is typically larger, not smaller) that budgeting for the primary is
    /// the correct common case; see phase-05 spec.
    pub fn context_window(&self) -> u32 {
        self.primary.context_window()
    }

    /// Highest tier this router can serve without ever leaving the local machine.
    /// A local llama.cpp primary caps out at `Thinking` — `Ultra` is cloud-effective-only
    /// (see the [`Tier`] doc) and a local backend has no second model to escalate to, so
    /// asking it for `Ultra` would silently collapse back to the same session. An
    /// all-cloud config has no such ceiling: every cloud model name is reachable, so the
    /// cap is `Ultra`. Consumed by `EscalationPolicy` (Phases 4, 6) to decide whether an
    /// escalation step is reachable at all before spending a retry attempting it.
    pub fn highest_local_tier(&self) -> Tier {
        #[cfg(feature = "llama")]
        {
            // Only a `llama` build can ever have a llama.cpp primary — without the
            // feature this branch is unreachable dead code, so the cap never applies.
            if self.primary.provider_name() == "llama.cpp" {
                return Tier::Thinking;
            }
        }
        Tier::Ultra
    }

    /// Operator's cost/quality dial, clamped to `0..=10` regardless of what value
    /// slipped into `LlmConfig` (e.g. a stray out-of-range preference) — clamping here
    /// rather than at `LlmConfig` construction keeps the stored preference lossless
    /// while guaranteeing every consumer sees an in-range value.
    pub fn cost_quality(&self) -> u8 {
        self.cost_quality
    }

    /// Tries the cloud fallback (if configured) after a pre-first-token failure on
    /// whichever backend `stream_backend` was streaming from; returns `backend_err`
    /// unchanged if there is no fallback configured. `failed_backend_name` is used only
    /// for the log line — the fallback client itself is always `self.fallback`, never
    /// parameterized (see `stream_backend`'s doc for why). Never called once that
    /// backend has emitted a token — see `stream_backend`'s FALLBACK SCOPE comment.
    async fn fallback_stream_or_err(
        &self,
        req: CompletionRequest,
        failed_backend_name: &str,
        backend_err: anyhow::Error,
    ) -> Result<mpsc::Receiver<StreamChunk>> {
        match &self.fallback {
            Some(fallback) => {
                tracing::warn!(
                    "trying cloud fallback stream after {failed_backend_name} pre-first-token failure"
                );
                fallback.complete_stream(req).await
            }
            None => Err(backend_err),
        }
    }

    /// Streams from `backend` (either `self.primary` via `complete_stream`, or a
    /// `tier_clients` entry via `complete_stream_tiered`), applying the same
    /// peek-first-item fallback contract either way.
    ///
    /// FALLBACK SCOPE (red-team constraint, phase-06; extraction, not duplication):
    /// falls back to `self.fallback` — the cloud fallback, never a parameterized
    /// substitute — ONLY when `backend` fails before emitting a single token: either by
    /// returning `Err` from `complete_stream` itself, or by the FIRST item off its
    /// channel being `StreamChunk::Error`. Once any `Token` has been forwarded, a later
    /// stream error is relayed as-is and the fallback is never consulted — re-running
    /// the whole request through a second backend at that point would re-stream the
    /// answer and duplicate user-visible text. This holds identically whether `backend`
    /// is the primary or a tier override: the fallback client is always `self.fallback`.
    async fn stream_backend(
        &self,
        backend: &dyn LlmClient,
        req: CompletionRequest,
    ) -> Result<mpsc::Receiver<StreamChunk>> {
        let backend_name = backend.provider_name().to_string();
        let mut backend_rx = match backend.complete_stream(req.clone()).await {
            Ok(rx) => rx,
            Err(err) => {
                tracing::warn!("LLM backend ({backend_name}) stream init failed: {err:#}");
                return self.fallback_stream_or_err(req, &backend_name, err).await;
            }
        };

        // Peek the first item to decide pre-first-token vs. post-first-token failure.
        match backend_rx.recv().await {
            None => {
                // Channel closed with no message at all — treat as a pre-first-token
                // init failure (same bucket as an immediate `Err`).
                let err = anyhow::anyhow!("{backend_name} stream closed before producing any output");
                tracing::warn!("LLM backend ({backend_name}) stream: {err:#}");
                self.fallback_stream_or_err(req, &backend_name, err).await
            }
            Some(StreamChunk::Error(msg)) => {
                let err = anyhow::anyhow!("{msg}");
                tracing::warn!("LLM backend ({backend_name}) stream failed before first token: {msg}");
                self.fallback_stream_or_err(req, &backend_name, err).await
            }
            Some(first @ StreamChunk::Token(_)) => {
                tracing::info!(backend = %backend_name, "streaming from LLM backend");
                Ok(relay_with_first_item(backend_name, backend_rx, first))
            }
            Some(first @ StreamChunk::Done { .. }) => {
                // Zero-token completion (e.g. empty response) — still a legitimate
                // stream, not a failure; forward as-is, no fallback.
                tracing::info!(backend = %backend_name, "streaming from LLM backend (empty output)");
                Ok(relay_with_first_item(backend_name, backend_rx, first))
            }
        }
    }

    /// Streams a request against a specific model `tier`. Falls back to `primary` (the
    /// default model) when `tier` is `None`, or when `Some(tier)` names a tier with no
    /// configured override — mirrors `complete_tiered`'s "wired but inert" contract, so
    /// callers may pass a tier freely with zero behavior change until an operator
    /// configures `LlmConfig::tier_models`. Applies the exact same pre-first-token
    /// fallback-to-cloud contract as `complete_stream` — see `stream_backend`'s doc.
    pub async fn complete_stream_tiered(
        &self,
        tier: Option<Tier>,
        req: CompletionRequest,
    ) -> Result<mpsc::Receiver<StreamChunk>> {
        let backend = tier
            .and_then(|t| self.tier_clients.get(&t))
            .map(Arc::as_ref)
            .unwrap_or(&*self.primary);
        self.stream_backend(backend, req).await
    }
}

/// Bound on the router's own passthrough channel — mirrors the bound each backend
/// uses internally (llama: 64, cloud: 32); the router just relays, so any bound in
/// that ballpark is fine (KISS — no need to match exactly).
const ROUTER_STREAM_BOUND: usize = 64;

/// Re-emits `first` (already pulled off `src` to inspect it) followed by the rest of
/// `src`, on a fresh bounded channel. Runs as a spawned forwarding task so the
/// caller can return the receiver immediately without holding `src` open in the
/// trait method's own stack frame.
fn relay_with_first_item(
    backend_name: String,
    mut src: mpsc::Receiver<StreamChunk>,
    first: StreamChunk,
) -> mpsc::Receiver<StreamChunk> {
    let (tx, rx) = mpsc::channel(ROUTER_STREAM_BOUND);
    tokio::spawn(async move {
        if tx.send(first).await.is_err() {
            return; // consumer dropped — nothing left to relay
        }
        while let Some(chunk) = src.recv().await {
            if tx.send(chunk).await.is_err() {
                break;
            }
        }
        tracing::debug!(backend = %backend_name, "primary LLM stream relay finished");
    });
    rx
}

#[async_trait]
impl LlmClient for LlmRouter {
    async fn complete(&self, req: CompletionRequest) -> Result<String> {
        match self.primary.complete(req.clone()).await {
            Ok(text) => Ok(text),
            Err(primary_err) => {
                if let Some(fallback) = &self.fallback {
                    tracing::warn!(
                        "primary LLM ({}) failed: {primary_err:#}; trying cloud fallback",
                        self.primary.provider_name()
                    );
                    fallback.complete(req).await
                } else {
                    Err(primary_err)
                }
            }
        }
    }

    /// See `stream_backend`'s FALLBACK SCOPE doc — this is `stream_backend` applied to
    /// `self.primary` specifically; `complete_stream_tiered` is the tier-routed sibling.
    async fn complete_stream(&self, req: CompletionRequest) -> Result<mpsc::Receiver<StreamChunk>> {
        self.stream_backend(&*self.primary, req).await
    }

    fn provider_name(&self) -> &str {
        self.primary.provider_name()
    }

    fn context_window(&self) -> u32 {
        self.primary.context_window()
    }
}

#[cfg(test)]
mod tier_tests {
    //! `complete_tiered` fallback semantics (phase-07 tier foundation). The mock
    //! server below echoes back the `model` field it received in the request body
    //! as the completion text — the only reliable way to prove which cloud model
    //! name a given call actually used (both tiers speak through the same
    //! `CloudClient` HTTP shape, so `provider_name()` alone can't distinguish them).
    use super::*;
    use crate::Message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Minimal OpenAI-compatible responder: reads the request body, extracts the
    /// `"model"` field, and returns it verbatim as the completion content.
    async fn spawn_model_echo_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request_text = String::from_utf8_lossy(&buf[..n]);
                    let body_start = request_text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                    let model = serde_json::from_str::<serde_json::Value>(&request_text[body_start..])
                        .ok()
                        .and_then(|v| v["model"].as_str().map(str::to_string))
                        .unwrap_or_else(|| "unknown".to_string());

                    let payload = serde_json::json!({
                        "choices": [{ "message": { "content": model } }]
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        format!("http://{addr}")
    }

    // `..LlmConfig::default()` is needed when the `llama` feature adds its fields, but
    // needless when it's off — a feature-conditional false positive for this lint.
    #[allow(clippy::needless_update)]
    fn tiered_config(base_url: String) -> LlmConfig {
        LlmConfig {
            cloud_api_keys: vec!["test-key".to_string()],
            cloud_base_url: base_url,
            cloud_model: "default-model".to_string(),
            tier_models: TierModels {
                fast: Some(TierEndpoint::inherit("fast-model")),
                medium: None,
                thinking: None,
                ultra: None,
            },
            ..LlmConfig::default()
        }
    }

    #[tokio::test]
    async fn complete_tiered_uses_the_configured_tier_model() {
        let base_url = spawn_model_echo_server().await;
        let router = LlmRouter::init(tiered_config(base_url)).await;

        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        let text = router.complete_tiered(Some(Tier::Fast), req).await.expect("completion");

        assert_eq!(text, "fast-model", "Some(configured tier) must route to its override model");
    }

    /// Always responds with `content` verbatim, ignoring the request body. Distinguishes
    /// "which server handled the call" from "which model name was sent" — the model-echo
    /// server can't prove a per-tier `base_url` override because the model name rides in
    /// the request body regardless of endpoint.
    async fn spawn_fixed_content_server(content: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let _ = stream.read(&mut buf).await;
                    let payload =
                        serde_json::json!({ "choices": [{ "message": { "content": content } }] })
                            .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// Echoes back the bearer token from the `Authorization` header as the completion
    /// text — proves which API key a call actually sent (the non-stream path uses
    /// `.bearer_auth(key)`, i.e. `Authorization: Bearer <token>`).
    async fn spawn_auth_echo_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request_text = String::from_utf8_lossy(&buf[..n]);
                    let token = request_text
                        .split("Bearer ")
                        .nth(1)
                        .and_then(|rest| rest.split(['\r', '\n']).next())
                        .unwrap_or("no-token")
                        .to_string();
                    let payload =
                        serde_json::json!({ "choices": [{ "message": { "content": token } }] })
                            .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn tier_endpoint_with_own_base_url_routes_to_its_own_server() {
        let default_url = spawn_model_echo_server().await;
        let tier_url = spawn_fixed_content_server("HIT-TIER-SERVER").await;
        #[allow(clippy::needless_update)]
        let config = LlmConfig {
            cloud_api_keys: vec!["default-key".to_string()],
            cloud_base_url: default_url,
            cloud_model: "default-model".to_string(),
            tier_models: TierModels {
                fast: Some(TierEndpoint {
                    model: "fast-model".to_string(),
                    base_url: Some(tier_url),
                    api_keys: None, // inherits default-key
                }),
                ..TierModels::default()
            },
            ..LlmConfig::default()
        };
        let router = LlmRouter::init(config).await;

        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        let text = router
            .complete_tiered(Some(Tier::Fast), req)
            .await
            .expect("completion");
        assert_eq!(
            text, "HIT-TIER-SERVER",
            "Fast tier must hit its OWN base_url, not the default endpoint"
        );

        // The default path (tier=None) still routes to the default endpoint.
        let req2 = CompletionRequest::simple(vec![Message::user("hi")]);
        let text2 = router
            .complete_tiered(None, req2)
            .await
            .expect("completion");
        assert_eq!(text2, "default-model");
    }

    #[tokio::test]
    async fn tier_endpoint_with_own_api_keys_sends_its_own_key() {
        let url = spawn_auth_echo_server().await;
        #[allow(clippy::needless_update)]
        let config = LlmConfig {
            cloud_api_keys: vec!["default-key".to_string()],
            cloud_base_url: url,
            cloud_model: "default-model".to_string(),
            tier_models: TierModels {
                ultra: Some(TierEndpoint {
                    model: "big".to_string(),
                    base_url: None, // inherits the (single) test endpoint
                    api_keys: Some(vec!["ultra-key".to_string()]),
                }),
                ..TierModels::default()
            },
            ..LlmConfig::default()
        };
        let router = LlmRouter::init(config).await;

        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        let text = router
            .complete_tiered(Some(Tier::Ultra), req)
            .await
            .expect("completion");
        assert_eq!(
            text, "ultra-key",
            "Ultra tier must send its OWN api key, not the inherited default"
        );
    }

    #[tokio::test]
    async fn complete_tiered_falls_back_to_default_when_tier_is_none() {
        let base_url = spawn_model_echo_server().await;
        let router = LlmRouter::init(tiered_config(base_url)).await;

        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        let text = router.complete_tiered(None, req).await.expect("completion");

        assert_eq!(text, "default-model", "tier=None must use the default model");
    }

    #[tokio::test]
    async fn complete_tiered_falls_back_to_default_when_tier_is_unconfigured() {
        let base_url = spawn_model_echo_server().await;
        let router = LlmRouter::init(tiered_config(base_url)).await;

        // `medium` has no override in `tiered_config` — must fall back, not error.
        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        let text = router.complete_tiered(Some(Tier::Medium), req).await.expect("completion");

        assert_eq!(text, "default-model", "tier=Some(unconfigured) must fall back to the default model");
    }

    #[tokio::test]
    async fn tier_with_no_resolvable_keys_is_absent_from_snapshot() {
        // A tier that names a model but has neither its own keys nor a default pool is
        // unbuildable — `build_tier_clients` skips it and routing falls back to primary.
        // The snapshot must NOT name it (else the turn-meta badge would report a model the
        // router never uses); a tier WITH its own keys is still reported even when defaults
        // are empty.
        #[allow(clippy::needless_update)]
        let config = LlmConfig {
            cloud_api_keys: vec![], // no default key pool
            cloud_base_url: "http://127.0.0.1:1".to_string(),
            cloud_model: "default-model".to_string(),
            tier_models: TierModels {
                ultra: Some(TierEndpoint {
                    model: "big".to_string(),
                    base_url: None,
                    api_keys: None, // resolves empty (own None + default empty)
                }),
                fast: Some(TierEndpoint {
                    model: "fast-model".to_string(),
                    base_url: None,
                    api_keys: Some(vec!["own-key".to_string()]), // resolvable on its own
                }),
                ..TierModels::default()
            },
            ..LlmConfig::default()
        };
        let snap = LlmRouter::init(config).await.snapshot();
        assert_eq!(
            snap.model_for_tier(Some(Tier::Ultra)),
            "default-model",
            "unbuildable tier must report the default model, mirroring the routing fallback"
        );
        assert_eq!(
            snap.model_for_tier(Some(Tier::Fast)),
            "fast-model",
            "a tier with its own keys is reported even when default keys are empty"
        );
    }

    #[tokio::test]
    async fn complete_tiered_propagates_primary_error_when_no_backend_configured() {
        // No cloud keys at all — primary is `NoopClient`, which always errors. Both
        // paths (tier and no-tier) must surface that same error, not panic or hang.
        let router = LlmRouter::init(LlmConfig {
            cloud_api_keys: vec![],
            ..LlmConfig::default()
        })
        .await;

        let req = CompletionRequest::simple(vec![Message::user("hi")]);
        let err = router.complete_tiered(Some(Tier::Fast), req).await.expect_err("no backend configured");
        assert!(err.to_string().contains("Chưa cấu hình LLM"));
    }

    /// CRITICAL regression (Phase 3): with NO tier overrides configured,
    /// `complete_tiered(None, ..)` and `complete_tiered(Some(any_tier), ..)` must both
    /// route to the exact same default model as plain `complete(..)` — zero behavior
    /// change until an operator opts in. The echo server returns the model name used, so
    /// equal text == same model routed.
    #[tokio::test]
    async fn unconfigured_tiers_route_identically_to_default() {
        let base_url = spawn_model_echo_server().await;
        // No tier_models at all — the default (unconfigured) shape.
        #[allow(clippy::needless_update)]
        let config = LlmConfig {
            cloud_api_keys: vec!["test-key".to_string()],
            cloud_base_url: base_url,
            cloud_model: "default-model".to_string(),
            ..LlmConfig::default()
        };
        let router = LlmRouter::init(config).await;

        let baseline = router
            .complete(CompletionRequest::simple(vec![Message::user("hi")]))
            .await
            .expect("baseline");
        for tier in [None, Some(Tier::Fast), Some(Tier::Medium), Some(Tier::Thinking), Some(Tier::Ultra)] {
            let text = router
                .complete_tiered(tier, CompletionRequest::simple(vec![Message::user("hi")]))
                .await
                .expect("tiered");
            assert_eq!(text, baseline, "unconfigured tier {tier:?} must match default routing");
        }
    }
}

#[cfg(test)]
mod resolution_tests {
    //! Pure tier-ordering, model→tier resolution, and snapshot tests (no network).
    use super::*;

    #[test]
    fn tier_ordinal_ordering_is_fast_lt_medium_lt_thinking_lt_ultra() {
        assert!(Tier::Fast < Tier::Medium);
        assert!(Tier::Medium < Tier::Thinking);
        assert!(Tier::Thinking < Tier::Ultra);
        // Sanity: the derive keys off declaration order — a reorder would break this.
        let mut tiers = [Tier::Ultra, Tier::Fast, Tier::Thinking, Tier::Medium];
        tiers.sort();
        assert_eq!(tiers, [Tier::Fast, Tier::Medium, Tier::Thinking, Tier::Ultra]);
    }

    #[test]
    fn tier_next_steps_up_then_saturates_at_ultra() {
        assert_eq!(Tier::Fast.next(), Some(Tier::Medium));
        assert_eq!(Tier::Medium.next(), Some(Tier::Thinking));
        assert_eq!(Tier::Thinking.next(), Some(Tier::Ultra));
        assert_eq!(Tier::Ultra.next(), None);
    }

    #[test]
    fn resolve_model_tier_exact_match_beats_substring() {
        // The substring-bug case: `gpt-5.4` must bind to its OWN Thinking entry, never
        // fuzzy-hit the longer `gpt-5.4-mini` (Fast).
        assert_eq!(resolve_model_tier("gpt-5.4", &[]), Some(Tier::Thinking));
        assert_eq!(resolve_model_tier("gpt-5.4-mini", &[]), Some(Tier::Fast));
    }

    #[test]
    fn resolve_model_tier_substring_matches_dated_deployment_ids() {
        // A dated deployment id has no exact entry; the longest curated substring wins.
        assert_eq!(
            resolve_model_tier("gpt-4o-mini-2024-07-18", &[]),
            Some(Tier::Fast),
            "must prefer the longer `gpt-4o-mini` key over the shorter `gpt-4o`"
        );
    }

    #[test]
    fn resolve_model_tier_unknown_is_none() {
        assert_eq!(resolve_model_tier("some-unlisted-model-x", &[]), None);
    }

    #[test]
    fn resolve_model_tier_user_overrides_win_and_are_exact_first() {
        let overrides = vec![
            ("my-local-3b".to_string(), Tier::Fast),
            ("gpt-4o".to_string(), Tier::Ultra), // re-tier a curated model
        ];
        assert_eq!(resolve_model_tier("my-local-3b", &overrides), Some(Tier::Fast));
        // Override's exact match beats the curated table's own exact `gpt-4o`→Medium.
        assert_eq!(resolve_model_tier("gpt-4o", &overrides), Some(Tier::Ultra));
    }

    #[test]
    fn resolve_model_tier_override_wins_by_substring_over_longer_curated_key() {
        // Review LOW1: a shorter operator override must beat a longer curated substring key.
        // `mycorp-gpt-4o-mini` contains both the override `mycorp` (Ultra) and the curated
        // `gpt-4o-mini` (Fast); the override must win even though its key is shorter.
        let overrides = vec![("mycorp".to_string(), Tier::Ultra)];
        assert_eq!(resolve_model_tier("mycorp-gpt-4o-mini", &overrides), Some(Tier::Ultra));
        // With no override, the curated substring still resolves (fallback intact).
        assert_eq!(resolve_model_tier("mycorp-gpt-4o-mini", &[]), Some(Tier::Fast));
    }

    #[tokio::test]
    async fn snapshot_reports_default_and_overridden_tier_models() {
        #[allow(clippy::needless_update)]
        let config = LlmConfig {
            cloud_api_keys: vec!["k".to_string()],
            cloud_base_url: "http://127.0.0.1:1".to_string(),
            cloud_model: "gpt-4o".to_string(),
            tier_models: TierModels {
                fast: Some(TierEndpoint::inherit("gpt-4o-mini")),
                medium: None,
                thinking: None,
                ultra: Some(TierEndpoint::inherit("claude-opus-4")),
            },
            ..LlmConfig::default()
        };
        let router = LlmRouter::init(config).await;
        let snap = router.snapshot();

        assert_eq!(snap.model_for_tier(None), "gpt-4o", "None → default model");
        assert_eq!(snap.model_for_tier(Some(Tier::Fast)), "gpt-4o-mini");
        assert_eq!(snap.model_for_tier(Some(Tier::Ultra)), "claude-opus-4");
        // A tier with no override falls back to the default model.
        assert_eq!(snap.model_for_tier(Some(Tier::Medium)), "gpt-4o");
        // Session tier is derived from the default model (`gpt-4o` → Medium in the table).
        assert_eq!(snap.session_tier(&[]), Some(Tier::Medium));
    }

    #[test]
    fn cost_quality_defaults_to_seven() {
        assert_eq!(LlmConfig::default().cost_quality, DEFAULT_COST_QUALITY);
    }

    #[tokio::test]
    async fn cost_quality_clamps_to_ten_and_in_range_values_round_trip() {
        #[allow(clippy::needless_update)]
        let over_range = LlmConfig { cost_quality: 11, ..LlmConfig::default() };
        let router = LlmRouter::init(over_range).await;
        assert_eq!(router.cost_quality(), 10, "11 must clamp down to the max of 10");

        #[allow(clippy::needless_update)]
        let in_range = LlmConfig { cost_quality: 3, ..LlmConfig::default() };
        let router = LlmRouter::init(in_range).await;
        assert_eq!(router.cost_quality(), 3, "an in-range value must round-trip unchanged");
    }
}

#[cfg(test)]
mod stream_backend_tests {
    //! `stream_backend`/`complete_stream_tiered`/`highest_local_tier` — exercised with
    //! an in-process scripted `LlmClient` double rather than a real HTTP/SSE server, so
    //! these tests assert the peek+relay+fallback *shape* directly without needing to
    //! match either backend's wire format. Direct `LlmRouter { .. }` construction (not
    //! `LlmRouter::init`) is used to wire up scripted primary/fallback/tier clients —
    //! legal here because these tests are a submodule of `router`, which owns the
    //! struct's private fields.
    use super::*;
    use crate::Message;

    /// Either fails `complete_stream` immediately (`init_err`), or streams a fixed
    /// script of `StreamChunk`s over a real channel — so `stream_backend`'s peek/relay
    /// logic runs unmodified against a deterministic source.
    struct ScriptedClient {
        name: &'static str,
        init_err: Option<&'static str>,
        chunks: Vec<StreamChunk>,
    }

    impl ScriptedClient {
        fn ok(name: &'static str, chunks: Vec<StreamChunk>) -> Arc<dyn LlmClient> {
            Arc::new(Self { name, init_err: None, chunks })
        }
        fn failing(name: &'static str, err: &'static str) -> Arc<dyn LlmClient> {
            Arc::new(Self { name, init_err: Some(err), chunks: vec![] })
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn complete(&self, _req: CompletionRequest) -> Result<String> {
            Ok(self.name.to_string())
        }

        async fn complete_stream(&self, _req: CompletionRequest) -> Result<mpsc::Receiver<StreamChunk>> {
            if let Some(msg) = self.init_err {
                return Err(anyhow::anyhow!(msg));
            }
            let (tx, rx) = mpsc::channel(16);
            for chunk in self.chunks.clone() {
                tx.send(chunk).await.expect("test channel has capacity for its own fixed script");
            }
            Ok(rx)
        }

        fn provider_name(&self) -> &str {
            self.name
        }

        fn context_window(&self) -> u32 {
            4096
        }
    }

    fn req() -> CompletionRequest {
        CompletionRequest::simple(vec![Message::user("hi")])
    }

    async fn drain(mut rx: mpsc::Receiver<StreamChunk>) -> Vec<StreamChunk> {
        let mut out = Vec::new();
        while let Some(chunk) = rx.recv().await {
            out.push(chunk);
        }
        out
    }

    fn token(s: &str) -> StreamChunk {
        StreamChunk::Token(s.to_string())
    }

    fn done() -> StreamChunk {
        StreamChunk::Done { total_tokens: 1, prompt_tokens: None }
    }

    /// Builds a router with scripted backends and no tier overrides — the shared base
    /// for tests that only care about `primary`/`fallback` behavior.
    fn router_with(primary: Arc<dyn LlmClient>, fallback: Option<Arc<dyn LlmClient>>) -> LlmRouter {
        router_with_tiers(primary, fallback, HashMap::new())
    }

    fn router_with_tiers(
        primary: Arc<dyn LlmClient>,
        fallback: Option<Arc<dyn LlmClient>>,
        tier_clients: HashMap<Tier, Arc<dyn LlmClient>>,
    ) -> LlmRouter {
        LlmRouter {
            primary,
            fallback,
            tier_clients,
            default_model: "default-model".to_string(),
            tier_model_names: HashMap::new(),
            cost_quality: DEFAULT_COST_QUALITY,
        }
    }

    /// CRITICAL: with no tier configured, `complete_stream_tiered(None, ..)` must be
    /// byte-equivalent to plain `complete_stream(..)` — same backend, same chunks.
    #[tokio::test]
    async fn complete_stream_tiered_none_matches_complete_stream() {
        let script = vec![token("hello"), done()];

        let via_plain = drain(
            router_with(ScriptedClient::ok("primary", script.clone()), None)
                .complete_stream(req())
                .await
                .expect("plain stream"),
        )
        .await;
        let via_tiered = drain(
            router_with(ScriptedClient::ok("primary", script), None)
                .complete_stream_tiered(None, req())
                .await
                .expect("tiered stream"),
        )
        .await;

        assert_eq!(via_plain, via_tiered);
    }

    /// CRITICAL: `Some(configured tier)` routes to that tier's client; `Some(unconfigured
    /// tier)` falls back to `primary` — mirrors `complete_tiered`'s non-streaming contract.
    #[tokio::test]
    async fn complete_stream_tiered_selects_configured_tier_and_falls_back_when_unconfigured() {
        let mut tier_clients: HashMap<Tier, Arc<dyn LlmClient>> = HashMap::new();
        tier_clients.insert(Tier::Fast, ScriptedClient::ok("fast-tier", vec![token("fast"), done()]));
        let router = router_with_tiers(ScriptedClient::ok("primary", vec![token("primary"), done()]), None, tier_clients);

        let fast = drain(router.complete_stream_tiered(Some(Tier::Fast), req()).await.expect("fast stream")).await;
        assert_eq!(fast, vec![token("fast"), done()], "Some(configured) must route to the tier client");

        let unconfigured = drain(
            router
                .complete_stream_tiered(Some(Tier::Medium), req())
                .await
                .expect("unconfigured-tier stream"),
        )
        .await;
        assert_eq!(unconfigured, vec![token("primary"), done()], "Some(unconfigured) must fall back to primary");
    }

    /// HIGH: a pre-first-token failure on a TIER client triggers the exact same
    /// cloud-fallback branch a primary failure would — `fallback_stream_or_err` never
    /// distinguishes which backend it was called for.
    #[tokio::test]
    async fn tier_client_pre_first_token_failure_falls_back_to_cloud_like_primary_does() {
        let mut tier_clients: HashMap<Tier, Arc<dyn LlmClient>> = HashMap::new();
        tier_clients.insert(Tier::Thinking, ScriptedClient::failing("thinking-tier", "tier init failed"));
        let router = router_with_tiers(
            ScriptedClient::ok("primary", vec![token("primary"), done()]),
            Some(ScriptedClient::ok("cloud-fallback", vec![token("fallback"), done()])),
            tier_clients,
        );

        let out = drain(
            router
                .complete_stream_tiered(Some(Tier::Thinking), req())
                .await
                .expect("fallback stream"),
        )
        .await;
        assert_eq!(out, vec![token("fallback"), done()], "tier failure must reach the same cloud fallback as primary");
    }

    /// HIGH: once a tier client has emitted its first token, a later error on that same
    /// stream relays as-is — the fallback is never consulted post-first-token.
    #[tokio::test]
    async fn tier_client_post_first_token_error_relays_as_is() {
        let mut tier_clients: HashMap<Tier, Arc<dyn LlmClient>> = HashMap::new();
        tier_clients.insert(
            Tier::Thinking,
            ScriptedClient::ok("thinking-tier", vec![token("partial"), StreamChunk::Error("mid-stream drop".into())]),
        );
        let router = router_with_tiers(
            ScriptedClient::ok("primary", vec![token("primary"), done()]),
            Some(ScriptedClient::ok("cloud-fallback", vec![token("fallback"), done()])),
            tier_clients,
        );

        let out = drain(
            router
                .complete_stream_tiered(Some(Tier::Thinking), req())
                .await
                .expect("stream returned despite later error"),
        )
        .await;
        assert_eq!(
            out,
            vec![token("partial"), StreamChunk::Error("mid-stream drop".into())],
            "post-first-token error must relay as-is, never substituted by the fallback"
        );
    }

    /// HIGH: cloud-only primary has no local ceiling — `Ultra` is reachable.
    #[tokio::test]
    async fn highest_local_tier_is_ultra_under_cloud_only_primary() {
        let router = router_with(ScriptedClient::ok("cloud", vec![token("x"), done()]), None);
        assert_eq!(router.highest_local_tier(), Tier::Ultra);
    }

    /// HIGH: a llama.cpp primary caps out at `Thinking` — only meaningful (and only
    /// compiled) when the `llama` feature's branch of `highest_local_tier` is active;
    /// `cargo test -p haily-llm --features llama` covers this arm.
    #[cfg(feature = "llama")]
    #[tokio::test]
    async fn highest_local_tier_is_thinking_under_llama_primary() {
        let router = router_with(ScriptedClient::ok("llama.cpp", vec![token("x"), done()]), None);
        assert_eq!(router.highest_local_tier(), Tier::Thinking);
    }
}
