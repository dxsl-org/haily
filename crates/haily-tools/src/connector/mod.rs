//! Connector seam — the external-side abstraction the action journal drives.
//!
//! Phase 3 provided the `ConnectorExecutor` trait + a test mock; phase 4 adds the generic,
//! manifest-interpreting `HttpConnectorTool` + `HttpExecutor` (raw HTTP through the SSRF
//! allowance guard) + the `Manifest`/`OpSpec` schema. Phase 5 proved an Odoo-specific
//! `OdooExecutor` end-to-end against a live sandbox; Phase 4a authored the Odoo manifest's
//! `auth`+`protocol` sections to reproduce that behavior on the GENERIC `HttpExecutor` and
//! retired `OdooExecutor` — Odoo is now the first protocol-config, not a bespoke executor.
//! `odoo_fault` (fault-shape extraction) stays: it is still the generic path's own reader
//! (`HttpExecutor::outcome_from_parsed`), never Odoo-executor-specific.
pub mod credential;
pub mod executor;
pub mod http_connector_tool;
pub mod manifest;
pub mod odoo_fault;
pub mod protocol;
pub mod readback_diff;
pub mod redact;

pub use credential::CredentialGetter;
pub use executor::{ConnectorExecutor, ExecOutcome};
pub use http_connector_tool::{HttpConnectorTool, HttpExecutor, HttpExecutorConfig};
pub use manifest::{
    approved_version_pref_key, check_version, manifest_diff, AuthSpec, FaultRule, Manifest,
    ManifestDiff, MethodShape, ModelRequiredFields, OpDiff, OpSpec, ProtocolSpec, ReadbackSpec,
    ResolvedAuthScheme, VersionCheck,
};
pub use protocol::ConnectionOverlay;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// Fail-closed placeholder executor registered until phase 4 injects the real HTTP impl.
/// Every method errors, so a `journal_undo`/reconcile driven by it can NEVER silently
/// pretend a write happened — it surfaces "connector not configured" instead.
pub struct UnconfiguredExecutor;

#[async_trait]
impl ConnectorExecutor for UnconfiguredExecutor {
    async fn call(&self, _op: &str, _params: &Value) -> Result<ExecOutcome> {
        anyhow::bail!("connector not configured (phase 4 wires the HTTP executor)")
    }
    async fn read_back(
        &self,
        _op: &str,
        _correlation_ref: &str,
        _model_hint: Option<&str>,
        _id_hint: Option<&str>,
    ) -> Result<Value> {
        anyhow::bail!("connector not configured (phase 4 wires the HTTP executor)")
    }
}
