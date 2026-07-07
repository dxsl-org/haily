//! Thin app-layer surface for the GUI's connector config UI (Phase 7, "Assistant Depth").
//!
//! [`summary`] is the read side: lists installed manifests (any status, so a `disabled` one
//! can be re-enabled) plus a re-approval banner built on the ALREADY-DEFINED-BUT-NEVER-
//! EXERCISED `manifest::approved_version_pref_key`/`manifest_diff` convention (Safe Operator
//! Harness phase 4's M1). [`admin`] is the write side: exactly the two admin actions this
//! phase adds — setting a connector's credential (via [`crate::CredentialStore::set_credential`],
//! never plaintext) and toggling `status` — plus acknowledging a re-approval banner. NEITHER
//! side writes `manifest_json`/`content_hash`: manifest authoring stays human/test-only via
//! `connectors::insert_version` (m3), untouched by this module.
//!
//! **Revocation liveness:** `register_connectors` reads `connector_manifests` ONLY at
//! startup (`haily-core::lib.rs`), so [`admin::set_connector_status`] disabling a connector
//! does NOT take effect until the next restart. This module does not attempt to hot-reload
//! the registry (out of scope, real restructuring); the GUI surfaces the restart requirement
//! instead of implying instant revocation (see the phase's Deviation Log for why journaling
//! the admin action into `action_journal` was rejected in favor of this simpler path).
mod admin;
mod summary;

pub use admin::{acknowledge_connector_version, set_connector_credential, set_connector_status};
pub use summary::{list_connectors, ConnectorSummary, ReapprovalState};
