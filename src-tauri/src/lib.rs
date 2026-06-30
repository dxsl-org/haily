/// Tauri IPC bridge — thin layer between Svelte frontend and haily-core.
///
/// Startup sequence (in setup hook):
///   DB → KMS → Orchestrator → GuiAdapter → AdapterManager →
///   ProactiveDaemon → dispatch task → chunk→event bridge task
use haily_db::{queries::meta, DbHandle};
use haily_io::{AdapterManager, GuiRequestSender, Request, ResponseChunk};
use haily_kms::KmsHandle;
use haily_llm::{LlmConfig, PromptFormat};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

struct AppState {
    gui_req_tx: GuiRequestSender,
    db: Arc<DbHandle>,
    orc: Arc<haily_core::Orchestrator>,
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Send a user message. Returns the session UUID so the frontend can correlate
/// incoming `haily-chunk` events to this conversation turn.
#[tauri::command]
async fn send_message(
    message: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let session_id = Uuid::new_v4();
    let req = Request {
        session_id,
        adapter_id: "gui".to_string(),
        message,
        user_ref: None,
    };
    state
        .gui_req_tx
        .send(req)
        .await
        .map_err(|e| e.to_string())?;
    Ok(session_id.to_string())
}

/// Re-read LLM preferences from DB and hot-swap the active LLM backend.
/// Call after saving any llm.* preference so changes take effect immediately.
///
/// Returns the active provider name ("llama.cpp", cloud model, or "unconfigured")
/// so the UI can distinguish a real model load from a silent fallback — the router
/// never errors on load, it falls back to `NoopClient` ("unconfigured") instead.
#[tauri::command]
async fn reload_llm(state: State<'_, AppState>) -> Result<String, String> {
    let cfg = load_llm_config(&state.orc.kms).await;
    state.orc.reload_llm(cfg).await;
    Ok(state.orc.llm_provider())
}

/// Return all stored user preferences as a flat key→value map.
#[tauri::command]
async fn get_preferences(
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let prefs = meta::all_preferences(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let map: serde_json::Map<String, serde_json::Value> = prefs
        .into_iter()
        .map(|p| (p.key, serde_json::Value::String(p.value)))
        .collect();
    Ok(serde_json::Value::Object(map))
}

/// Scan <exe_dir>/models/ for GGUF files and return metadata for the UI.
/// Multi-part files (part 2, 3, …) are hidden — only the -00001- entry point
/// is shown; llama.cpp loads all parts automatically.
#[tauri::command]
fn list_local_models() -> Vec<serde_json::Value> {
    let models_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("models")))
        .unwrap_or_else(|| std::path::PathBuf::from("models"));

    let Ok(entries) = std::fs::read_dir(&models_dir) else { return vec![] };

    let mut models: Vec<serde_json::Value> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()?.to_str()? != "gguf" { return None; }
            let name = path.file_name()?.to_str()?.to_string();
            // Hide continuation parts (e.g. -00002-of-00003)
            if name.contains("-of-") && !name.contains("-00001-of-") { return None; }
            let lower = name.to_lowercase();
            let format = if lower.contains("gemma") { "gemma4" } else { "chatml" };
            Some(serde_json::json!({
                "name": name,
                "path": path.to_string_lossy(),
                "format": format,
            }))
        })
        .collect();

    models.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    models
}

/// Persist a single preference key/value (source = "gui").
#[tauri::command]
async fn set_preference(
    key: String,
    value: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    meta::upsert_preference(&state.db, &key, &value, "gui")
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

async fn init_stack(data_dir: &std::path::Path) -> anyhow::Result<(Arc<DbHandle>, Arc<KmsHandle>, Arc<haily_core::Orchestrator>)> {
    let db_path = data_dir.join("haily.db");
    let db = DbHandle::init(&db_path).await?;
    let kms = KmsHandle::init(db.clone()).await?;
    let db = Arc::new(db);
    let kms = Arc::new(kms);
    let llm_cfg = load_llm_config(&kms).await;
    let orc = Arc::new(haily_core::Orchestrator::init(kms.clone(), db.clone(), llm_cfg).await?);
    Ok((db, kms, orc))
}

async fn load_llm_config(kms: &KmsHandle) -> LlmConfig {
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

    pref!("llm.cloud_base_url", cfg.cloud_base_url);
    pref!("llm.cloud_model",    cfg.cloud_model);

    // Multi-key: stored as JSON array. Backward compat: fall back to single-key.
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

    if let Ok(Some(path)) = meta::get_preference(db, "llm.llama_model_path").await {
        cfg.llama_model_path = Some(std::path::PathBuf::from(path));
    }
    if let Ok(Some(fmt)) = meta::get_preference(db, "llm.llama_prompt_format").await {
        cfg.llama_prompt_format = PromptFormat::from_str(&fmt);
    }
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

    if cfg.cloud_api_keys.is_empty() {
        for env_key in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "HAILY_CLOUD_KEY"] {
            if let Ok(v) = std::env::var(env_key) {
                cfg.cloud_api_keys.push(v);
            }
        }
    }
    cfg
}

/// Dispatch loop: pulls requests from adapter channels and fans out to the orchestrator.
async fn run_dispatch(am: AdapterManager, orc: Arc<haily_core::Orchestrator>) -> anyhow::Result<()> {
    use tokio::sync::mpsc;
    let (req_tx, mut req_rx) = mpsc::channel::<Request>(64);
    am.start_all(req_tx).await?;

    while let Some(req) = req_rx.recv().await {
        let session_id = req.session_id;
        am.bind_session(session_id, &req.adapter_id);

        let (resp_tx, mut resp_rx) = mpsc::channel::<ResponseChunk>(256);
        let orc_c = Arc::clone(&orc);
        let am_c = am.clone();

        tokio::spawn(async move {
            let delivery = {
                let am = am_c.clone();
                tokio::spawn(async move {
                    while let Some(chunk) = resp_rx.recv().await {
                        let done = matches!(chunk, ResponseChunk::Complete);
                        am.deliver(session_id, chunk).await.ok();
                        if done { break; }
                    }
                })
            };
            // Clone before move so we can send an error message if process() fails.
            let resp_tx_err = resp_tx.clone();
            if let Err(e) = orc_c.process(req, resp_tx).await {
                tracing::error!("orchestrator error: {e:#}");
                resp_tx_err.send(ResponseChunk::Text(format!("⚠️ {e:#}"))).await.ok();
                resp_tx_err.send(ResponseChunk::Complete).await.ok();
            }
            delivery.await.ok();
            am_c.unbind_session(&session_id);
        });
    }
    Ok(())
}

fn data_dir() -> std::path::PathBuf {
    // Portable-first: store data next to the exe in ./data/
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("data")))
        .unwrap_or_else(|| std::path::PathBuf::from("data"))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let data_dir = data_dir();
            std::fs::create_dir_all(&data_dir)?;

            let (db, _kms, orc) = tauri::async_runtime::block_on(init_stack(&data_dir))
                .map_err(|e| Box::new(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;

            let (gui_adapter, gui_req_tx, gui_resp_rx) = haily_io::GuiAdapter::new();
            let am = AdapterManager::builder()
                .register(Arc::new(gui_adapter))
                .build();

            // Start proactive daemon inside the Tokio runtime context.
            // setup() is sync; tokio::spawn inside start() requires an active reactor.
            let daemon = haily_proactive::ProactiveDaemon::new(db.clone(), am.clone());
            tauri::async_runtime::spawn(async move { daemon.start(); });

            app.manage(AppState { gui_req_tx, db: db.clone(), orc: Arc::clone(&orc) });

            // Agent dispatch runs in background
            let am_c = am.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = run_dispatch(am_c, orc).await {
                    tracing::error!("dispatch loop exited: {e:#}");
                }
            });

            // Forward GuiAdapter response chunks → Tauri events so Svelte can stream them
            let ah: AppHandle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut rx = gui_resp_rx;
                while let Some((session_id, chunk)) = rx.recv().await {
                    let _ = ah.emit(
                        "haily-chunk",
                        serde_json::json!({
                            "session_id": session_id.to_string(),
                            "chunk": chunk,
                        }),
                    );
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            send_message,
            get_preferences,
            set_preference,
            list_local_models,
            reload_llm,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Haily");
}
