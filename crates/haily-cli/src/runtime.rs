/// Shared startup, LLM config loading, and the request dispatch loop.
use anyhow::Result;
use haily_core::Orchestrator;
use haily_db::{queries::meta, DbHandle};
use haily_io::{AdapterManager, Request, ResponseChunk};
use haily_kms::KmsHandle;
use haily_llm::LlmConfig;
#[cfg(feature = "llama")]
use haily_llm::PromptFormat;
use std::{path::Path, sync::Arc};
use tokio::sync::mpsc;
use tracing::info;

/// Initialise DB → KMS → Orchestrator. Cheap to clone since SqlitePool is Arc-backed.
pub async fn init(data_dir: &Path) -> Result<(Arc<DbHandle>, Arc<KmsHandle>, Arc<Orchestrator>)> {
    let db_path = data_dir.join("haily.db");
    info!("DB: {}", db_path.display());

    // `DbHandle` is Clone; KMS takes its own clone of the pool.
    let db = DbHandle::init(&db_path).await?;
    info!("KMS: building index from DB…");
    let kms = KmsHandle::init(db.clone()).await?;

    let db = Arc::new(db);
    let kms = Arc::new(kms);
    let llm_cfg = load_llm_config(&kms).await;

    let orc = Arc::new(
        Orchestrator::init(Arc::clone(&kms), Arc::clone(&db), llm_cfg).await?,
    );

    info!(llm = orc.llm_provider(), "ready");
    Ok((db, kms, orc))
}

/// Load LLM routing config from KMS preferences, falling back to env vars then defaults.
pub async fn load_llm_config(kms: &KmsHandle) -> LlmConfig {
    let db = kms.db();
    let mut cfg = LlmConfig::default();

    macro_rules! pref {
        ($key:literal, $field:expr) => {
            if let Ok(Some(v)) = meta::get_preference(db, $key).await {
                $field = v;
            }
        };
        ($key:literal, $field:expr, opt) => {
            if let Ok(Some(v)) = meta::get_preference(db, $key).await {
                $field = Some(v);
            }
        };
    }

    pref!("llm.ollama_url",       cfg.ollama_url);
    pref!("llm.ollama_model",     cfg.ollama_model);
    pref!("llm.cloud_base_url",   cfg.cloud_base_url);
    pref!("llm.cloud_model",      cfg.cloud_model);
    pref!("llm.cloud_api_key",    cfg.cloud_api_key, opt);

    // Embedded llama.cpp config (only active when `llama` feature is compiled in).
    #[cfg(feature = "llama")]
    {
        if let Ok(Some(path)) = meta::get_preference(db, "llm.llama_model_path").await {
            cfg.llama_model_path = Some(std::path::PathBuf::from(path));
        }
        if let Ok(Some(fmt)) = meta::get_preference(db, "llm.llama_prompt_format").await {
            cfg.llama_prompt_format = PromptFormat::from_str(&fmt);
        }
    }

    // Env vars override preferences (useful for Docker / CI)
    for key in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "HAILY_CLOUD_KEY"] {
        if let Ok(v) = std::env::var(key) {
            if cfg.cloud_api_key.is_none() {
                cfg.cloud_api_key = Some(v);
            }
        }
    }

    cfg
}

/// Receive requests from adapters and dispatch each to the orchestrator concurrently.
///
/// Each request gets its own response channel. ResponseChunks are forwarded to the
/// originating adapter via AdapterManager as they arrive.
pub async fn dispatch_loop(am: AdapterManager, orc: Arc<Orchestrator>) -> Result<()> {
    let (req_tx, mut req_rx) = mpsc::channel::<Request>(64);
    am.start_all(req_tx).await?;
    info!("dispatch loop running");

    while let Some(req) = req_rx.recv().await {
        let session_id = req.session_id;
        am.bind_session(session_id, &req.adapter_id);

        let (resp_tx, mut resp_rx) = mpsc::channel::<ResponseChunk>(256);
        let orc_clone = Arc::clone(&orc);
        let am_clone = am.clone();

        tokio::spawn(async move {
            // Forward chunks from orchestrator → adapter while the agent loop runs.
            let delivery = {
                let am = am_clone.clone();
                tokio::spawn(async move {
                    while let Some(chunk) = resp_rx.recv().await {
                        let done = matches!(chunk, ResponseChunk::Complete);
                        am.deliver(session_id, chunk).await.ok();
                        if done { break; }
                    }
                })
            };

            if let Err(e) = orc_clone.process(req, resp_tx).await {
                tracing::error!("orchestrator error: {e:#}");
            }

            delivery.await.ok();
            am_clone.unbind_session(&session_id);
        });
    }

    Ok(())
}
