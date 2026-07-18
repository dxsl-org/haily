//! View Engine (Phase A) — the core-side in-memory store for [`haily_types::DataView`]
//! snapshots. See `store.rs` for `ViewStore`.

mod store;

pub use store::ViewStore;
