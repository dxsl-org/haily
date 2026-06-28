/// Tauri IPC bridge — thin layer between Svelte frontend and haily-core.
///
/// Startup sequence (in setup hook):
///   DB → KMS → Orchestrator → GuiAdapter → AdapterManager →
///   ProactiveDaemon → dispatch task → chunk→event bridge task
use haily_db::{queries::meta, DbHandle};
use haily_io::{AdapterManager, GuiRequestSender, Request, ResponseChunk};
use haily_kms::KmsHandle;
use haily_llm::LlmConfig;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

struct AppState {
    gui_req_tx: GuiRequestSender,
    db: Arc<DbHandle>,
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

/// List model names available in the running Ollama instance.
/// Returns an empty list if Ollama is unreachable.
#[tauri::command]
async fn list_ollama_models(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let url = meta::get_preference(&state.db, "llm.ollama_url")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "http://localhost:11434".to_string());

    #[derive(serde::Deserialize)]
    struct TagsResponse { models: Vec<ModelEntry> }
    #[derive(serde::Deserialize)]
    struct ModelEntry { name: String }

    let resp = reqwest::Client::new()
        .get(format!("{url}/api/tags"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let tags: TagsResponse = resp.json().await.map_err(|e| e.to_string())?;
    Ok(tags.models.into_iter().map(|m| m.name).collect())
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

    pref!("llm.ollama_url",     cfg.ollama_url);
    pref!("llm.ollama_model",   cfg.ollama_model);
    pref!("llm.cloud_base_url", cfg.cloud_base_url);
    pref!("llm.cloud_model",    cfg.cloud_model);
    pref!("llm.cloud_api_key",  cfg.cloud_api_key, opt);

    for key in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "HAILY_CLOUD_KEY"] {
        if let Ok(v) = std::env::var(key) {
            if cfg.cloud_api_key.is_none() {
                cfg.cloud_api_key = Some(v);
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
            if let Err(e) = orc_c.process(req, resp_tx).await {
                tracing::error!("orchestrator error: {e:#}");
            }
            delivery.await.ok();
            am_c.unbind_session(&session_id);
        });
    }
    Ok(())
}

fn data_dir() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("haily")
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

            app.manage(AppState { gui_req_tx, db: db.clone() });

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
            list_ollama_models,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Haily");
}
