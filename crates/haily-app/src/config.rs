//! LLM routing config loader — the single implementation shared by every mode.
//!
//! Consolidates the two previously-divergent copies (`haily-cli/src/runtime.rs` and
//! `src-tauri/src/lib.rs`). The `llama_n_ctx` field on `LlmConfig` is itself
//! `#[cfg(feature = "llama")]`-gated in `haily-llm` (router.rs) — the old src-tauri
//! copy read `llm.llama_n_ctx` unconditionally, which only happened to compile because
//! src-tauri always builds with the `llama` feature enabled. Reading it here without
//! the matching `#[cfg(feature = "llama")]` guard would break `--no-default-features`
//! builds (e.g. `haily-cli` without `--features llama`), so the gate is preserved.
use haily_db::queries::meta;
use haily_kms::KmsHandle;
use haily_llm::LlmConfig;
#[cfg(feature = "llama")]
use haily_llm::PromptFormat;

/// Load LLM routing config from KMS preferences, falling back to env vars then defaults.
///
/// Never fails — a missing or malformed preference simply leaves the corresponding
/// `LlmConfig` field at its default. Callers should not log the returned config's
/// `cloud_api_keys` field verbatim (security: key material must never hit `tracing`).
pub async fn load_llm_config(kms: &KmsHandle) -> LlmConfig {
    let db = kms.db();
    let mut cfg = LlmConfig::default();

    macro_rules! pref {
        ($key:literal, $field:expr) => {
            if let Ok(Some(v)) = meta::get_preference(db, $key).await {
                $field = v;
            }
        };
    }

    pref!("llm.cloud_base_url", cfg.cloud_base_url);
    pref!("llm.cloud_model", cfg.cloud_model);

    // Multi-key: stored as JSON array under `llm.cloud_api_keys`.
    // Backward compat: fall back to the old single-key `llm.cloud_api_key`.
    if let Ok(Some(json)) = meta::get_preference(db, "llm.cloud_api_keys").await {
        if let Ok(keys) = serde_json::from_str::<Vec<String>>(&json) {
            cfg.cloud_api_keys = keys;
        }
    }
    if cfg.cloud_api_keys.is_empty() {
        if let Ok(Some(key)) = meta::get_preference(db, "llm.cloud_api_key").await {
            if !key.is_empty() {
                cfg.cloud_api_keys = vec![key];
            }
        }
    }

    // Embedded llama.cpp config — only active when the `llama` feature is compiled in.
    // ALL llama-specific reads (including n_ctx) must stay inside this one gate: every
    // field here does not exist on `LlmConfig` at all without the feature.
    #[cfg(feature = "llama")]
    {
        if let Ok(Some(path)) = meta::get_preference(db, "llm.llama_model_path").await {
            cfg.llama_model_path = Some(std::path::PathBuf::from(path));
        }
        if let Ok(Some(fmt)) = meta::get_preference(db, "llm.llama_prompt_format").await {
            cfg.llama_prompt_format = PromptFormat::from_name(&fmt);
        }
        // GPU layers: explicit override wins; otherwise keep the compile-time auto-detected default.
        if let Ok(Some(v)) = meta::get_preference(db, "llm.llama_n_gpu_layers").await {
            if let Ok(n) = v.parse::<u32>() {
                cfg.llama_n_gpu_layers = n;
            }
        }
        if let Ok(Some(v)) = meta::get_preference(db, "llm.llama_n_ctx").await {
            if let Ok(n) = v.parse::<u32>() {
                cfg.llama_n_ctx = n;
            }
        }
    }

    // Env var fallback (useful for Docker / CI). Only applies if no keys were found in DB.
    if cfg.cloud_api_keys.is_empty() {
        for env_key in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "HAILY_CLOUD_KEY"] {
            if let Ok(v) = std::env::var(env_key) {
                cfg.cloud_api_keys.push(v);
            }
        }
    }

    cfg
}
