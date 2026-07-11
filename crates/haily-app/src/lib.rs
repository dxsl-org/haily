//! Shared application bootstrap, dispatch loop, and graceful shutdown.
//!
//! One implementation reused by every deployment mode (CLI REPL, headless daemon,
//! Tauri GUI) — see `AppHandle::bootstrap`. This crate depends on `haily-io` (adapter
//! trait + manager) so it must sit above `haily-core` in the dependency graph;
//! `haily-core` itself stays io-free per the workspace's layering invariant.
mod auto_approve;
mod config;
pub mod connector_config;
pub mod eval;
pub mod credential_store;
mod dispatch;
mod turns;
mod watchers;

pub mod bootstrap;

pub use auto_approve::{load_auto_approve, validate_auto_approve};
pub use bootstrap::{export_database, AppHandle, BootstrapOptions};
pub use config::{load_llm_config, load_odoo_api_key, ODOO_API_KEY_PREF};
pub use credential_store::{CredentialPolicy, CredentialStore};
pub use eval::run_coding_eval_all;
pub use turns::TurnRegistry;
pub use watchers::list_work_items_status;

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
