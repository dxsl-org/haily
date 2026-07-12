/// Tauri IPC bridge — thin layer between Svelte frontend and haily-app. Setup:
/// `AppHandle::bootstrap` builds the full stack in one call. Shutdown:
/// `RunEvent::ExitRequested` → `AppHandle::shutdown`, bounded by `SHUTDOWN_TIMEOUT`.
mod models;

use haily_app::connector_config::{self, ConnectorSummary};
use haily_app::{
    AppHandle, BootstrapOptions, CredentialStore, PendingApproval, SkillView, TurnRegistry,
    WorkspaceView,
};
use haily_db::{
    queries::{journal, meta},
    DbHandle,
};
use haily_io::{
    Adapter, ApprovalResolver, DepthMode, GuiProactiveReceiver, GuiRequestSender,
    GuiResponseReceiver, GuiRunEventReceiver, GuiWorkItemsReceiver, Request, WorkItemStatus,
};
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
    /// OS-keyring-backed credential store (Harness Completion phase 4). Cloned out of
    /// `app` at setup, mirroring `approval_resolver`/`turns`/`kill`, so the connector
    /// config UI's credential-set command (Phase 7) doesn't need to lock `app`.
    credential_store: Arc<CredentialStore>,
    app: Mutex<Option<AppHandle>>,
}

/// Preference key holding the GUI depth toggle's current mode (phase 7). Read per
/// message so a toggle change takes effect on the next message with no restart; a VN/EN
/// depth phrase in the message itself still overrides this per-turn (server-side, in
/// `run_turn`).
const DEPTH_PREF_KEY: &str = "ui.depth_mode";

#[tauri::command]
async fn send_message(message: String, state: State<'_, AppState>) -> Result<String, String> {
    let session_id = Uuid::new_v4();
    // The toggle's persisted mode seeds this turn; unset/unknown → Normal. A phrase in
    // `message` may still override it server-side (never the other way around).
    let depth = meta::get_preference(&state.db, DEPTH_PREF_KEY)
        .await
        .ok()
        .flatten()
        .map(|v| DepthMode::from_label(&v))
        .unwrap_or_default();
    let req = Request {
        session_id,
        adapter_id: "gui".to_string(),
        message,
        user_ref: None,
        depth,
        origin: Default::default(),
    };
    state.gui_req_tx.send(req).await.map_err(|e| e.to_string())?;
    Ok(session_id.to_string())
}

/// Persist the GUI depth toggle (phase 7). Stores a normalized label; an unknown value
/// falls back to `normal` (never `deep` — Deep is only ever set by an exact match, so a
/// malformed value can't silently escalate the 3–5× cost path). Takes effect on the next
/// message (see `send_message`); Deep is never auto-selected by the harness.
#[tauri::command]
async fn set_depth(mode: String, state: State<'_, AppState>) -> Result<(), String> {
    let normalized = DepthMode::from_label(&mode).as_label();
    meta::upsert_preference(&state.db, DEPTH_PREF_KEY, normalized, "gui")
        .await
        .map_err(|e| e.to_string())
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

/// Current active work items (queued/running/paused/interrupted), for the work-items
/// panel's on-mount reconcile (Phase 5). Pure delegation to `haily_app::list_work_items_status`
/// — this file stays glue-only, all conversion logic lives in the app layer.
#[tauri::command]
async fn list_work_items(state: State<'_, AppState>) -> Result<Vec<WorkItemStatus>, String> {
    haily_app::list_work_items_status(&state.db).await.map_err(|e| e.to_string())
}

/// List installed connectors for the config UI (Phase 7) — read-only, delegates entirely to
/// the app layer; no manifest write path here.
#[tauri::command]
async fn list_connectors(state: State<'_, AppState>) -> Result<Vec<ConnectorSummary>, String> {
    connector_config::list_connectors(&state.db).await.map_err(|e| e.to_string())
}

/// Set/rotate a connector's credential. HUMAN-only path — no registered `Tool` reaches this,
/// so the agent/LLM loop can never call it. Writes straight to the OS keyring via
/// `CredentialStore::set_credential`, which also scrubs any overwritten plaintext's
/// WAL/freelist residue (M5c) — the secret is never recoverable from the DB file.
#[tauri::command]
async fn set_connector_credential(
    cred_ref: String,
    secret: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    connector_config::set_connector_credential(&state.credential_store, &cred_ref, &secret)
        .await
        .map_err(|e| e.to_string())
}

/// Enable/disable a connector manifest version. Takes effect at the NEXT restart only — the
/// registry loads active manifests once at startup (`haily-core::lib.rs`); this command does
/// not hot-reload it. The frontend must surface that restart requirement rather than imply
/// instant revocation (see the phase's Deviation Log for why this was chosen over journaling
/// the admin action).
#[tauri::command]
async fn set_connector_status(
    id: String,
    status: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    connector_config::set_connector_status(&state.db, &id, &status).await.map_err(|e| e.to_string())
}

/// Record that a human has explicitly reviewed and accepted a connector's live manifest
/// version, clearing its re-approval banner. Never touches `manifest_json`/`content_hash`.
#[tauri::command]
async fn acknowledge_connector_version(
    connector_name: String,
    version: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    connector_config::acknowledge_connector_version(&state.db, &connector_name, &version)
        .await
        .map_err(|e| e.to_string())
}

/// Skills browser (phase 11a): authored kit-pack + synthesized skills, each with its
/// persisted enable/pin state. Read-only; pure delegation to the app layer.
#[tauri::command]
async fn list_skills(state: State<'_, AppState>) -> Result<Vec<SkillView>, String> {
    haily_app::list_skills(&state.db, &state.kms).await.map_err(|e| e.to_string())
}

/// Enable/disable a skill (phase 11a). Persists the admin state; see the app layer's
/// `cockpit` module doc for the deferred-enforcement contract (mirrors `set_connector_status`).
#[tauri::command]
async fn set_skill_enabled(name: String, enabled: bool, state: State<'_, AppState>) -> Result<(), String> {
    haily_app::set_skill_enabled(&state.db, &name, enabled).await.map_err(|e| e.to_string())
}

/// Pin/unpin a skill (phase 11a). Persists the admin state; enforcement is deferred (see
/// `list_skills`).
#[tauri::command]
async fn pin_skill(name: String, pinned: bool, state: State<'_, AppState>) -> Result<(), String> {
    haily_app::pin_skill(&state.db, &name, pinned).await.map_err(|e| e.to_string())
}

/// Active coding workspaces (phase 11a): branch, dirty status, and the host sandbox
/// posture (incl. the non-enforcing `NullSandbox` warning flag). Read-only.
#[tauri::command]
async fn list_workspaces(state: State<'_, AppState>) -> Result<Vec<WorkspaceView>, String> {
    haily_app::list_workspaces(&state.db).await.map_err(|e| e.to_string())
}

/// Discard a coding workspace (phase 11a) — revert the worktree, remove it, delete the
/// branch, soft-delete the row. SESSION-SCOPED: `session_id` (from the `WorkspaceView` the
/// frontend is acting on) is the boundary — a foreign id returns `false`, never discarding
/// another session's workspace. Returns `false` (not an error) if no active workspace matched.
#[tauri::command]
async fn discard_workspace(
    id: String,
    session_id: String,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    haily_app::discard_workspace(&state.db, &id, &session_id).await.map_err(|e| e.to_string())
}

/// The unified diff of a workspace's worktree against HEAD, for the DiffViewer's read side
/// (phase 11a). SESSION-SCOPED like `discard_workspace`. The ACCEPT side is NOT here —
/// accepting a run's changes routes through the existing `worktree_apply` tool approval
/// (`approve_tool`), view + accept only, no editor. Returns `null` if the id is
/// unknown/foreign; the diff text is untrusted repo content the frontend renders as data.
#[tauri::command]
async fn workspace_diff(
    id: String,
    session_id: String,
    state: State<'_, AppState>,
) -> Result<Option<String>, String> {
    haily_app::workspace_diff(&state.db, &id, &session_id).await.map_err(|e| e.to_string())
}

/// The unified approvals queue's PENDING set (phase 11a): every in-flight tool approval
/// across all channels. Reconcile source only — the descriptive tool payload lives in the
/// `ToolApprovalRequest` chunk the frontend already received; each entry's `session_id` is
/// the auth boundary for resolving it via `approve_tool`. HISTORY is read separately via
/// `list_journal`. Locks `app` for the read (mirrors `reload_llm`) — infrequent, panel-driven.
#[tauri::command]
async fn list_approvals(state: State<'_, AppState>) -> Result<Vec<PendingApproval>, String> {
    let guard = state.app.lock().await;
    let app = guard.as_ref().ok_or("app is shutting down")?;
    Ok(app.pending_approvals())
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

/// Forward live work-item snapshots to the frontend as `haily-work-items` events.
///
/// `rx` is the latest-wins watch receiver (see `GuiWorkItemsReceiver`) — `changed()`
/// only resolves on an actual update, so this loop is idle between bursts, and
/// because the channel is single-slot, a rapid run of updates collapses to just the
/// final one delivered here (the earlier ones were never "missed", they were
/// superseded before this loop had a chance to read them — the intended coalesce
/// behavior, not a bug). Ends when the `GuiAdapter` (sender side) is dropped, which
/// only happens at process teardown.
fn spawn_work_items_bridge(ah: TauriAppHandle, mut rx: GuiWorkItemsReceiver) {
    tauri::async_runtime::spawn(async move {
        while rx.changed().await.is_ok() {
            let snapshot = rx.borrow_and_update().clone();
            let _ = ah.emit("haily-work-items", snapshot);
        }
    });
}

/// Forward live proactive-card snapshots to the frontend as `haily-proactive-cards`
/// events (phase 08). Same shape/lifecycle as `spawn_work_items_bridge` — `rx` is a
/// latest-wins watch receiver, so this loop is idle between updates and ends only
/// when the `GuiAdapter` (sender side) is dropped at process teardown. Unlike the
/// work-items snapshot, the VALUE itself is already accumulated/capped per-kind on
/// the `GuiAdapter` side (see `haily_io::gui::GuiProactiveReceiver`), so a value
/// observed here already reflects every still-live card, not just the latest event.
fn spawn_proactive_cards_bridge(ah: TauriAppHandle, mut rx: GuiProactiveReceiver) {
    tauri::async_runtime::spawn(async move {
        while rx.changed().await.is_ok() {
            let snapshot = rx.borrow_and_update().clone();
            let _ = ah.emit("haily-proactive-cards", snapshot);
        }
    });
}

/// Forward ordered pipeline `RunEvent`s to the frontend as `haily-run-events` events
/// (phase 11a). UNLIKE the work-item/proactive bridges above, `rx` is a bounded, ordered
/// `mpsc` (NOT a latest-wins `watch`): a coding run's event log must be delivered in full
/// and in order, so this drains every event rather than coalescing to the latest. Each
/// event was already tag-stripped at `AdapterManager::deliver_run_event`, so it is inert
/// data the frontend renders as-is. Ends when the `GuiAdapter` (sender) drops at teardown.
fn spawn_run_events_bridge(ah: TauriAppHandle, mut rx: GuiRunEventReceiver) {
    tauri::async_runtime::spawn(async move {
        while let Some((session_id, event)) = rx.recv().await {
            let payload = serde_json::json!({ "session_id": session_id.to_string(), "event": event });
            let _ = ah.emit("haily-run-events", payload);
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
            let (
                gui_adapter,
                gui_req_tx,
                gui_resp_rx,
                gui_work_items_rx,
                gui_proactive_rx,
                gui_run_events_rx,
            ) = haily_io::GuiAdapter::new();
            let adapters: Vec<Arc<dyn Adapter>> = vec![Arc::new(gui_adapter)];
            let bootstrap = AppHandle::bootstrap(&data_dir, adapters, BootstrapOptions::default());
            let app_handle = tauri::async_runtime::block_on(bootstrap)
                .map_err(|e| Box::new(std::io::Error::other(e.to_string())))?;
            let db = Arc::clone(&app_handle.db);
            let kms = Arc::clone(&app_handle.kms);
            let approval_resolver = app_handle.orchestrator.approval_resolver();
            let turns = app_handle.turn_registry();
            let kill = app_handle.orchestrator.kill_handle();
            let credential_store = Arc::clone(&app_handle.credential_store);
            app.manage(AppState {
                gui_req_tx,
                db,
                kms,
                approval_resolver,
                turns,
                kill,
                credential_store,
                app: Mutex::new(Some(app_handle)),
            });
            spawn_chunk_bridge(app.handle().clone(), gui_resp_rx);
            spawn_work_items_bridge(app.handle().clone(), gui_work_items_rx);
            spawn_proactive_cards_bridge(app.handle().clone(), gui_proactive_rx);
            spawn_run_events_bridge(app.handle().clone(), gui_run_events_rx);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            send_message,
            set_depth,
            approve_tool,
            cancel_turn,
            get_preferences,
            set_preference,
            list_local_models,
            reload_llm,
            list_journal,
            export_database,
            list_work_items,
            list_connectors,
            set_connector_credential,
            set_connector_status,
            acknowledge_connector_version,
            list_skills,
            set_skill_enabled,
            pin_skill,
            list_workspaces,
            discard_workspace,
            workspace_diff,
            list_approvals,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Haily")
        .run(|app_handle, event| {
            if let RunEvent::ExitRequested { .. } = event {
                handle_exit_requested(app_handle);
            }
        });
}
