//! LLM routing config loader — the single implementation shared by every mode.
//!
//! Consolidates the two previously-divergent copies (`haily-cli/src/runtime.rs` and
//! `src-tauri/src/lib.rs`). The `llama_n_ctx` field on `LlmConfig` is itself
//! `#[cfg(feature = "llama")]`-gated in `haily-llm` (router.rs) — the old src-tauri
//! copy read `llm.llama_n_ctx` unconditionally, which only happened to compile because
//! src-tauri always builds with the `llama` feature enabled. Reading it here without
//! the matching `#[cfg(feature = "llama")]` guard would break `--no-default-features`
//! builds (e.g. `haily-cli` without `--features llama`), so the gate is preserved.
use crate::credential_store::CredentialStore;
use haily_db::queries::meta;
use haily_kms::KmsHandle;
use haily_llm::{LlmConfig, TierEndpoint};
#[cfg(feature = "llama")]
use haily_llm::PromptFormat;
use serde::Deserialize;

/// A stored preference string mapped to `Some` only when non-empty — an empty tier
/// override must read as "no override" (`None`), not as a model literally named `""`.
fn non_empty(v: String) -> Option<String> {
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Wire shape of the per-tier endpoint preference `llm.tier.<tier>` (hybrid multi-model
/// config). `base_url`/`api_keys` are optional — absent or blank means "inherit the
/// session default" (`LlmConfig::cloud_base_url` / `cloud_api_keys`).
#[derive(Deserialize)]
struct TierPref {
    model: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_keys: Option<Vec<String>>,
}

/// Load one tier's endpoint override, preferring the new JSON schema
/// (`llm.tier.<tier>` = `{"model","base_url","api_keys"}`) and falling back to the legacy
/// plain-model-name pref (`llm.tier_model.<tier>`) for backward compatibility. A blank
/// model, blank base_url, or all-blank key list each normalize to `None` (inherit), so a
/// half-filled row never produces a model literally named `""` or an empty-string endpoint.
async fn load_tier_endpoint(
    db: &haily_db::DbHandle,
    json_key: &str,
    legacy_key: &str,
) -> Option<TierEndpoint> {
    // New schema: JSON blob. Only accept it when it parses AND names a non-blank model.
    if let Ok(Some(json)) = meta::get_preference(db, json_key).await {
        if !json.trim().is_empty() {
            if let Ok(tp) = serde_json::from_str::<TierPref>(&json) {
                let model = tp.model.trim().to_string();
                if !model.is_empty() {
                    // Trim before storing, not just before the emptiness check — the GUI
                    // already trims on save, but a pref written by hand/migration/future
                    // API must not persist surrounding whitespace into the model name or
                    // URL (a trailing space in base_url breaks the request URL silently).
                    let base_url = tp
                        .base_url
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    let api_keys = tp
                        .api_keys
                        .map(|ks| {
                            ks.into_iter()
                                .map(|k| k.trim().to_string())
                                .filter(|k| !k.is_empty())
                                .collect::<Vec<_>>()
                        })
                        .filter(|ks: &Vec<String>| !ks.is_empty());
                    return Some(TierEndpoint {
                        model,
                        base_url,
                        api_keys,
                    });
                }
            }
        }
    }
    // Legacy: plain model-name string; endpoint + keys inherit the session default.
    if let Ok(Some(v)) = meta::get_preference(db, legacy_key).await {
        return non_empty(v).map(TierEndpoint::inherit);
    }
    None
}

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

    // Cost/quality dial (Auto Model Routing R1, phase 7) — the single user-facing knob
    // (0 = cheapest, 10 = best). A garbage or out-of-range string must fall back to the
    // struct's own default (`DEFAULT_COST_QUALITY` = 7) rather than panic; `LlmRouter`
    // clamps again at construction, so this read only needs to guard the parse itself.
    if let Ok(Some(v)) = meta::get_preference(db, "llm.cost_quality").await {
        if let Ok(n) = v.parse::<u8>() {
            cfg.cost_quality = n;
        }
    }

    // Per-tier cloud endpoint overrides (hybrid multi-model config). New schema is a JSON
    // blob under `llm.tier.<tier>` carrying model + optional own base_url/api_keys;
    // legacy plain-model-name `llm.tier_model.<tier>` still loads (inherit endpoint+keys).
    // An absent/blank tier leaves it `None`, which `complete_tiered` treats as "use the
    // default model" — so routing is IDENTICAL to today until an operator sets at least one.
    cfg.tier_models.fast = load_tier_endpoint(db, "llm.tier.fast", "llm.tier_model.fast").await;
    cfg.tier_models.medium =
        load_tier_endpoint(db, "llm.tier.medium", "llm.tier_model.medium").await;
    cfg.tier_models.thinking =
        load_tier_endpoint(db, "llm.tier.thinking", "llm.tier_model.thinking").await;
    cfg.tier_models.ultra =
        load_tier_endpoint(db, "llm.tier.ultra", "llm.tier_model.ultra").await;

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

/// Preference key holding the Odoo connector's API key (Safe Operator Harness phase 5,
/// originally `kms_preferences`-only; Harness Completion phase 4 moves the actual secret
/// into the OS keyring under this same NAME as the cred-by-reference — see
/// [`crate::credential_store`]). Callers store only this key NAME as the journal
/// credential reference (C4) and read the secret by reference at call time — the secret
/// itself must never be logged or copied into a journal row.
pub const ODOO_API_KEY_PREF: &str = "connector.odoo.api_key";

/// Load the Odoo connector API key: keyring first (via `store`, cred-by-reference under
/// [`ODOO_API_KEY_PREF`]), then the `HAILY_ODOO_API_KEY` env var (useful for Docker/CI
/// where the bootstrap exports a freshly-generated scoped key). Returns `None` if unset in
/// both. `store` already implements the M5 read-fallback-to-plaintext-DB split internally —
/// this function does not duplicate that policy.
///
/// The returned string is the raw secret — the caller must NOT log it or persist it in a
/// journal row; only the preference key NAME ([`ODOO_API_KEY_PREF`]) is a safe reference (C4).
pub async fn load_odoo_api_key(store: &CredentialStore) -> Option<String> {
    if let Ok(Some(key)) = store.get_secret(ODOO_API_KEY_PREF).await {
        if !key.is_empty() {
            return Some(key);
        }
    }
    std::env::var("HAILY_ODOO_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_store::CredentialPolicy;
    use haily_db::DbHandle;
    use std::sync::Arc;

    fn use_mock_keyring() {
        keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
    }

    async fn store(dir: &std::path::Path) -> CredentialStore {
        let db = Arc::new(DbHandle::init(&dir.join("t.db")).await.unwrap());
        CredentialStore::new(db, CredentialPolicy::default())
    }

    async fn test_kms(dir: &std::path::Path) -> KmsHandle {
        let db = DbHandle::init(&dir.join("kms.db")).await.unwrap();
        KmsHandle::init(db, dir).await.unwrap()
    }

    #[tokio::test]
    async fn missing_cost_quality_preference_defaults_to_seven() {
        let dir = tempfile::tempdir().unwrap();
        let kms = test_kms(dir.path()).await;
        assert_eq!(load_llm_config(&kms).await.cost_quality, 7);
    }

    #[tokio::test]
    async fn garbage_cost_quality_preference_falls_back_to_seven_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let kms = test_kms(dir.path()).await;
        meta::upsert_preference(kms.db(), "llm.cost_quality", "not-a-number", "test")
            .await
            .unwrap();
        assert_eq!(load_llm_config(&kms).await.cost_quality, 7);
    }

    #[tokio::test]
    async fn valid_cost_quality_preference_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let kms = test_kms(dir.path()).await;
        meta::upsert_preference(kms.db(), "llm.cost_quality", "3", "test")
            .await
            .unwrap();
        assert_eq!(load_llm_config(&kms).await.cost_quality, 3);
    }

    #[tokio::test]
    async fn tier_json_schema_loads_model_and_own_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let kms = test_kms(dir.path()).await;
        meta::upsert_preference(
            kms.db(),
            "llm.tier.ultra",
            r#"{"model":"claude-opus-4","base_url":"https://api.anthropic.com","api_keys":["sk-ant-1","sk-ant-2"]}"#,
            "test",
        )
        .await
        .unwrap();
        let ep = load_llm_config(&kms).await.tier_models.ultra.unwrap();
        assert_eq!(ep.model, "claude-opus-4");
        assert_eq!(ep.base_url.as_deref(), Some("https://api.anthropic.com"));
        assert_eq!(
            ep.api_keys.as_deref(),
            Some(&["sk-ant-1".to_string(), "sk-ant-2".to_string()][..])
        );
    }

    #[tokio::test]
    async fn tier_json_with_only_a_model_inherits_endpoint_and_keys() {
        let dir = tempfile::tempdir().unwrap();
        let kms = test_kms(dir.path()).await;
        meta::upsert_preference(kms.db(), "llm.tier.fast", r#"{"model":"gpt-4o-mini"}"#, "test")
            .await
            .unwrap();
        let ep = load_llm_config(&kms).await.tier_models.fast.unwrap();
        assert_eq!(ep.model, "gpt-4o-mini");
        assert!(ep.base_url.is_none(), "absent base_url must inherit (None)");
        assert!(ep.api_keys.is_none(), "absent api_keys must inherit (None)");
    }

    #[tokio::test]
    async fn tier_json_blank_endpoint_and_key_fields_normalize_to_inherit() {
        let dir = tempfile::tempdir().unwrap();
        let kms = test_kms(dir.path()).await;
        meta::upsert_preference(
            kms.db(),
            "llm.tier.medium",
            r#"{"model":"m","base_url":"  ","api_keys":["","  "]}"#,
            "test",
        )
        .await
        .unwrap();
        let ep = load_llm_config(&kms).await.tier_models.medium.unwrap();
        assert_eq!(ep.model, "m");
        assert!(ep.base_url.is_none(), "blank base_url → inherit");
        assert!(ep.api_keys.is_none(), "all-blank key list → inherit");
    }

    #[tokio::test]
    async fn tier_json_surrounding_whitespace_is_trimmed_before_storing() {
        // The GUI already trims before saving, but a pref written by hand (DB edit,
        // migration, future API) must not persist stray whitespace into the model name
        // or endpoint — a trailing space in base_url silently breaks the request URL.
        let dir = tempfile::tempdir().unwrap();
        let kms = test_kms(dir.path()).await;
        meta::upsert_preference(
            kms.db(),
            "llm.tier.fast",
            r#"{"model":" gpt-4o-mini ","base_url":" https://api.openai.com ","api_keys":[" sk-1 "," sk-2 "]}"#,
            "test",
        )
        .await
        .unwrap();
        let ep = load_llm_config(&kms).await.tier_models.fast.unwrap();
        assert_eq!(ep.model, "gpt-4o-mini");
        assert_eq!(ep.base_url.as_deref(), Some("https://api.openai.com"));
        assert_eq!(
            ep.api_keys.as_deref(),
            Some(&["sk-1".to_string(), "sk-2".to_string()][..])
        );
    }

    #[tokio::test]
    async fn tier_json_blank_model_is_no_override() {
        let dir = tempfile::tempdir().unwrap();
        let kms = test_kms(dir.path()).await;
        meta::upsert_preference(kms.db(), "llm.tier.thinking", r#"{"model":"   "}"#, "test")
            .await
            .unwrap();
        assert!(load_llm_config(&kms).await.tier_models.thinking.is_none());
    }

    #[tokio::test]
    async fn legacy_plain_tier_model_pref_still_loads_as_inherit() {
        let dir = tempfile::tempdir().unwrap();
        let kms = test_kms(dir.path()).await;
        meta::upsert_preference(kms.db(), "llm.tier_model.fast", "gpt-4o-mini", "test")
            .await
            .unwrap();
        let ep = load_llm_config(&kms).await.tier_models.fast.unwrap();
        assert_eq!(ep.model, "gpt-4o-mini");
        assert!(ep.base_url.is_none());
        assert!(ep.api_keys.is_none());
    }

    #[tokio::test]
    async fn new_tier_json_wins_over_legacy_plain_pref() {
        let dir = tempfile::tempdir().unwrap();
        let kms = test_kms(dir.path()).await;
        meta::upsert_preference(kms.db(), "llm.tier_model.fast", "legacy-model", "test")
            .await
            .unwrap();
        meta::upsert_preference(kms.db(), "llm.tier.fast", r#"{"model":"new-model"}"#, "test")
            .await
            .unwrap();
        assert_eq!(
            load_llm_config(&kms).await.tier_models.fast.unwrap().model,
            "new-model"
        );
    }

    #[tokio::test]
    async fn odoo_api_key_read_from_preference_by_name() {
        use_mock_keyring();
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path()).await;
        // Unset: neither keyring/DB preference nor env → None.
        std::env::remove_var("HAILY_ODOO_API_KEY");
        assert!(load_odoo_api_key(&store).await.is_none());

        // A stored credential under the reference key name is returned verbatim,
        // regardless of which backing store (keyring vs. plaintext DB) it landed in.
        store
            .set_secret(ODOO_API_KEY_PREF, "sk-scoped-XYZ")
            .await
            .unwrap();
        assert_eq!(
            load_odoo_api_key(&store).await.as_deref(),
            Some("sk-scoped-XYZ")
        );
        // The reference name is the stable public constant (what the journal records, C4).
        assert_eq!(ODOO_API_KEY_PREF, "connector.odoo.api_key");
    }
}
