//! Shared application bootstrap, dispatch loop, and graceful shutdown.
//!
//! One implementation reused by every deployment mode (CLI REPL, headless daemon,
//! Tauri GUI) — see `AppHandle::bootstrap`. This crate depends on `haily-io` (adapter
//! trait + manager) so it must sit above `haily-core` in the dependency graph;
//! `haily-core` itself stays io-free per the workspace's layering invariant.
mod auto_approve;
pub mod cockpit;
mod config;
pub mod connector_config;
pub mod credential_store;
mod dispatch;
pub mod eval;
mod launch;
/// OS-toast notification seam (Unified Chat UI phase 7, D7). `pub` so the mode layer
/// (`src-tauri`) can implement `OsNotifier` for its real Tauri-backed notifier.
pub mod notify;
/// Desktop GUI's mobile pairing/devices command backing (Mobile Thin-Client plan phase 2b) —
/// pure delegation onto P2a's public `haily_io::mobile` API, no new persistence. Same feature
/// gate as the two modules above, for the same reason (references `haily_io::mobile::*` types).
#[cfg(feature = "mobile-server")]
pub mod mobile_admin;
/// Mobile-server config loader + DB-backed device store (Mobile Thin-Client plan phase 2a).
/// Gated behind the `mobile-server` feature (which forwards to `haily-io/mobile-server`, see
/// Cargo.toml) since both files reference `haily_io::mobile::*` types that only exist under
/// that feature — a default (no-feature) build must not even see this module. Registered here
/// as a minimal, additive edit purely so the two new source files this phase creates are part
/// of the crate's module tree — no other logic in this file changes. See the phase's
/// Deviation Log.
#[cfg(feature = "mobile-server")]
pub mod mobile_config;
#[cfg(feature = "mobile-server")]
pub mod mobile_device_store;
mod reaper;
/// Per-run kill/pause/resume control (Unified Chat UI phase 6, D3). `pub` so the mode layer
/// (`src-tauri`) can name `RunControlRegistry` for its own `AppState` field and call
/// `kill_run`/`pause_run`/`resume_run` directly.
pub mod run_control;
/// Runs-screen read surface (Unified Chat UI phase 7, D6). `pub` so the mode layer
/// (`src-tauri`) can name `RunSummary` for its own command return type.
pub mod runs_view;
mod session_transcript;
/// Data-driven slash-command registry (Unified Chat UI phase 2, D1). `pub` so the mode layer
/// (`src-tauri`) can name `SlashRegistry`/`SlashCommand` for its own `AppState` field and the
/// `list_slash_commands` command's return type.
pub mod slash_registry;
/// Dispatch-layer trigger resolver (Pipeline Activation & Wiring, phase 2) — slash + chat-intent
/// routing into the Phase 1 pipeline launcher. Not exported at the crate root: only
/// `dispatch.rs` (same crate) calls into it.
mod trigger;
mod turns;
mod watchers;

pub mod bootstrap;

pub use auto_approve::{load_auto_approve, validate_auto_approve};
pub use bootstrap::{export_database, AppHandle, BootstrapOptions};
pub use cockpit::{
    discard_workspace, list_skills, list_workspaces, pin_skill, set_skill_enabled,
    start_coding_run, workspace_diff, SkillView, WorkspaceView,
};
pub use config::{load_llm_config, load_odoo_api_key, ODOO_API_KEY_PREF};
pub use credential_store::{CredentialPolicy, CredentialStore};
pub use eval::run_coding_eval_all;
/// Re-exported so the mode layer (`src-tauri`) can name the approvals-queue snapshot type
/// without a direct `haily-core` dependency (phase 11a).
pub use haily_core::PendingApproval;
pub use launch::launch_coding_run;
pub use notify::{NoopNotifier, OsNotifier, ToastCoalescer};
#[cfg(feature = "mobile-server")]
pub use mobile_admin::{
    confirm_pair, list_devices, mobile_status, pairing_qr, pending_pairs, regenerate_cert,
    revoke_device, DeviceView, MobileStatusView, PendingPairView,
};
pub use run_control::RunControlRegistry;
pub use runs_view::{list_runs, RunSummary};
pub use slash_registry::{SlashCommand, SlashRegistry};
pub use turns::TurnRegistry;
pub use watchers::{list_work_items_status, spawn_distillation_bridge, spawn_run_event_bridge};

/// Default data directory, shared by every mode: `<exe_dir>/data/`.
///
/// Portable-first — Haily stores its DB next to the executable rather than an OS
/// user-data directory, so a copied install directory carries its data with it.
/// Falls back to a relative `data/` if `current_exe()` fails (e.g. under `cargo test`).
pub fn default_data_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("data")))
        .unwrap_or_else(|| std::path::PathBuf::from("data"))
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
