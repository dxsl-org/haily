//! Connector seam — the external-side abstraction the action journal drives.
//!
//! Phase 3 provides only the `ConnectorExecutor` trait + a test mock (moved forward from
//! phase 4 so `journal_undo` compiles, C-resequence). Phase 4 adds the generic HTTP impl
//! module here; phase 5 the Odoo impl. NO HTTP lives here yet.
pub mod executor;
pub mod redact;

pub use executor::{ConnectorExecutor, ExecOutcome};

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
    async fn read_back(&self, _op: &str, _correlation_ref: &str) -> Result<Value> {
        anyhow::bail!("connector not configured (phase 4 wires the HTTP executor)")
    }
}
