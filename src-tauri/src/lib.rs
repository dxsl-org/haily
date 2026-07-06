/// Tauri IPC bridge — thin layer between Svelte frontend and haily-app. Setup:
/// `AppHandle::bootstrap` builds the full stack in one call. Shutdown:
/// `RunEvent::ExitRequested` → `AppHandle::shutdown`, bounded by `SHUTDOWN_TIMEOUT`.
mod models;

use haily_app::{AppHandle, BootstrapOptions, TurnRegistry};
use haily_db::{
    queries::{journal, meta},
    DbHandle,
};
use haily_io::{Adapter, ApprovalResolver, GuiRequestSender, GuiResponseReceiver, Request};
use haily_kms::KmsHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{sync::Arc, time::Duration};
use tauri::{AppHandle as TauriAppHandle, Emitter, Manager, RunEvent, State};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Best-effort cleanup budget — Tauri's exit path has no guaranteed grace period.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Command-facing state. `app` is the shutdown surface — taken (leaving `None`) once
/// `ExitRequested` fires, so a second exit event can't double-shutdown a moved value.
struct AppState {
    gui_req_tx: GuiRequestSender,
    db: Arc<DbHandle>,
    kms: Arc<KmsHandle>,
    /// Resolves pending tool approvals — same broker `Orchestrator::process` awaits
    /// on. Cloned out of `app` once at setup so `approve_tool` doesn't need to lock
    /// `app` (which is also the shutdown surface) for every button tap.
    approval_resolver: Arc<dyn ApprovalResolver>,
    /// In-flight turn cancellation registry — same instance `dispatch.rs` registers
    /// every turn's token into. Cloned out of `app` once at setup, mirroring
    /// `approval_resolver`, so `cancel_turn` doesn't need to lock `app` either.
    turns: Arc<TurnRegistry>,
    /// `safety.disable_writes` kill switch (phase 3, C8) — the SAME `Arc<AtomicBool>` the
    /// orchestrator gates on. Cloned out of `app` at setup (mirrors `approval_resolver`/
    /// `turns`) because `set_preference` has NO orchestrator access (it is behind the
    /// shutdown `Mutex`). Flipping this Bool changes dispatch behavior live, no restart.
    kill: Arc<AtomicBool>,
    app: Mutex<Option<AppHandle>>,
}

#[tauri::command]
async fn send_message(message: String, state: State<'_, AppState>) -> Result<String, String> {
    let session_id = Uuid::new_v4();
    let req = Request { session_id, adapter_id: "gui".to_string(), message, user_ref: None };
    state.gui_req_tx.send(req).await.map_err(|e| e.to_string())?;
    Ok(session_id.to_string())
}

/// Resolve a pending tool approval raised for `session_id`. `session_id` is the
/// auth boundary (see `haily-core::approval`) — the frontend already has it from the
/// `ToolApprovalRequest` chunk's envelope (`ChunkPayload.session_id`), so this does
/// not need (and must not add) a global "current session" concept. Returns `false`
/// (not an error) if the approval was already resolved, unknown, or bound to a
/// different session — the caller should treat that as "nothing to do".
#[tauri::command]
async fn approve_tool(
    session_id: String,
    approval_id: String,
    approved: bool,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    let session_id = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    let approval_id = Uuid::parse_str(&approval_id).map_err(|e| e.to_string())?;
    Ok(state.approval_resolver.resolve(approval_id, session_id, approved))
}

/// Cancel the in-flight turn for `session_id`. Fires that turn's `CancellationToken`
/// (registered by `dispatch.rs` — see `haily_app::TurnRegistry`), which ends the
/// active LLM stream; the dispatch loop then still emits its normal terminal chunk
/// (`Complete` or `Error`), so the frontend's existing chunk handling closes the
/// bubble out with whatever text streamed before cancellation. Returns `false` (not
/// an error) if `session_id` has no in-flight turn — already finished, unknown, or
/// never started — which the caller should treat as a no-op.
#[tauri::command]
async fn cancel_turn(session_id: String, state: State<'_, AppState>) -> Result<bool, String> {
    let session_id = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    Ok(state.turns.cancel(session_id))
}

/// Re-read LLM preferences and hot-swap the active backend. Returns the active
/// provider name so the UI can tell a real model load from a silent "unconfigured"
/// fallback — the router never errors on load, only when a message is sent.
#[tauri::command]
async fn reload_llm(state: State<'_, AppState>) -> Result<String, String> {
    let cfg = haily_app::load_llm_config(&state.kms).await;
    let guard = state.app.lock().await;
    let app = guard.as_ref().ok_or("app is shutting down")?;
    app.orchestrator.reload_llm(cfg).await;
    Ok(app.orchestrator.llm_provider())
}

#[tauri::command]
async fn get_preferences(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let prefs = meta::all_preferences(&state.db).await.map_err(|e| e.to_string())?;
    let map: serde_json::Map<String, serde_json::Value> =
        prefs.into_iter().map(|p| (p.key, serde_json::Value::String(p.value))).collect();
    Ok(serde_json::Value::Object(map))
}

#[tauri::command]
fn list_local_models() -> Vec<serde_json::Value> {
    models::list_local_models()
}

/// Recent action-journal rows for the Safety tab's undo surface (phase 6). Each GUI turn
/// mints a fresh `session_id` (see `send_message`), so there is no single "current
/// session" to scope a recent-actions list to — the frontend instead tracks every
/// session id it has started this run and passes them here. Reuses `journal::list_by_session`
/// per id (no new query logic, per the phase-6 architecture note) and merges by recency;
/// an empty/unknown id list or an id with no rows both just contribute nothing, never an error.
#[tauri::command]
async fn list_journal(
    session_ids: Vec<String>,
    state: State<'_, AppState>,
) -> Result<Vec<journal::ActionJournalRow>, String> {
    let mut rows = Vec::new();
    for id in &session_ids {
        rows.extend(
            journal::list_by_session(&state.db, id)
                .await
                .map_err(|e| e.to_string())?,
        );
    }
    rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(rows)
}

/// Persist a preference AND, for `safety.disable_writes`, flip the runtime kill switch so
/// the change takes effect immediately (no restart). The Bool is the runtime source of
/// truth; the persisted row is only next-boot state — both are updated here because
/// `set_preference` has no orchestrator access (the kill Arc was cloned into `AppState`
/// at bootstrap for exactly this reason).
#[tauri::command]
async fn set_preference(key: String, value: String, state: State<'_, AppState>) -> Result<(), String> {
    if key == "safety.disable_writes" {
        let on = value == "true" || value == "1";
        state.kill.store(on, Ordering::Release);
    }
    meta::upsert_preference(&state.db, &key, &value, "gui").await.map_err(|e| e.to_string())
}

/// Manual "export database" action (Phase 6) — writes a consistent standalone copy to a
/// user-chosen path via the same `VACUUM INTO` mechanism the scheduled backup worker
/// uses. `dest_path` is picked by the frontend through `@tauri-apps/plugin-dialog`'s save
/// dialog; the frontend's dialog copy warns that the exported file is unencrypted and
/// contains all local data — this command performs no additional confirmation.
#[tauri::command]
async fn export_database(dest_path: String, state: State<'_, AppState>) -> Result<(), String> {
    state.db.backup_to(std::path::Path::new(&dest_path)).await.map_err(|e| e.to_string())
}

/// Forward `GuiAdapter` response chunks to the frontend as `haily-chunk` events.
fn spawn_chunk_bridge(ah: TauriAppHandle, mut rx: GuiResponseReceiver) {
    tauri::async_runtime::spawn(async move {
        while let Some((session_id, chunk)) = rx.recv().await {
            let payload = serde_json::json!({ "session_id": session_id.to_string(), "chunk": chunk });
            let _ = ah.emit("haily-chunk", payload);
        }
    });
}

/// Best-effort shutdown on exit. A hard kill (taskkill /F, power loss) skips this
/// entirely — SQLite WAL crash-safety is the real correctness backstop, not this path.
fn handle_exit_requested(app_handle: &TauriAppHandle) {
    let state = app_handle.state::<AppState>();
    let app = tauri::async_runtime::block_on(async { state.app.lock().await.take() });
    if let Some(app) = app {
        tauri::async_runtime::block_on(app.shutdown(SHUTDOWN_TIMEOUT));
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = haily_app::default_data_dir();
            std::fs::create_dir_all(&data_dir)?;
            let (gui_adapter, gui_req_tx, gui_resp_rx) = haily_io::GuiAdapter::new();
            let adapters: Vec<Arc<dyn Adapter>> = vec![Arc::new(gui_adapter)];
            let bootstrap = AppHandle::bootstrap(&data_dir, adapters, BootstrapOptions::default());
            let app_handle = tauri::async_runtime::block_on(bootstrap)
                .map_err(|e| Box::new(std::io::Error::other(e.to_string())))?;
            let db = Arc::clone(&app_handle.db);
            let kms = Arc::clone(&app_handle.kms);
            let approval_resolver = app_handle.orchestrator.approval_resolver();
            let turns = app_handle.turn_registry();
            let kill = app_handle.orchestrator.kill_handle();
            app.manage(AppState {
                gui_req_tx,
                db,
                kms,
                approval_resolver,
                turns,
                kill,
                app: Mutex::new(Some(app_handle)),
            });
            spawn_chunk_bridge(app.handle().clone(), gui_resp_rx);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            send_message,
            approve_tool,
            cancel_turn,
            get_preferences,
            set_preference,
            list_local_models,
            reload_llm,
            list_journal,
            export_database,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Haily")
        .run(|app_handle, event| {
            if let RunEvent::ExitRequested { .. } = event {
                handle_exit_requested(app_handle);
            }
        });
}
