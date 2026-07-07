//! Read side of the connector config UI (Phase 7) — see the parent module doc.
use anyhow::Result;
use haily_db::queries::connectors::{self, ConnectorManifestRow};
use haily_db::queries::meta;
use haily_db::DbHandle;
use haily_tools::connector::{manifest, Manifest, OpSpec};
use haily_tools::RiskTier;
use serde::Serialize;
use std::collections::HashMap;

/// One connector's read-only summary row for the config UI's list view.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectorSummary {
    pub id: String,
    pub connector_name: String,
    pub version: String,
    pub status: String,
    pub base_url_host: String,
    /// The highest [`RiskTier`] among this manifest's declared ops (fail-closed to
    /// `IrreversibleWrite` if the manifest cannot be parsed) — a single at-a-glance badge.
    pub risk_tier: String,
    /// The auth credential's reference name (e.g. `connector.odoo.api_key`), if this
    /// manifest declares one. `None` hides the credential form entirely — there is nothing
    /// to set.
    pub cred_ref: Option<String>,
    pub reapproval: Option<ReapprovalState>,
}

/// Surfaced when the live (most-recently-inserted) manifest version differs from the last
/// version a human explicitly acknowledged via `admin::acknowledge_connector_version`. Kept
/// snake_case (no camelCase rename) to match the nested [`manifest::ManifestDiff`], which has
/// none either — an outer/inner case mismatch would be more confusing than consistent
/// snake_case across this one DTO family.
#[derive(Debug, Clone, Serialize)]
pub struct ReapprovalState {
    pub approved_version: String,
    pub live_version: String,
    pub diff: manifest::ManifestDiff,
}

/// List every installed connector (latest version per `connector_name`, any status) with its
/// re-approval state. A connector seen for the FIRST time under this mechanism (no
/// `approved_version` preference on record at all) has its live version silently adopted as
/// the baseline rather than flagged — otherwise every pre-existing connector would show a
/// spurious "never approved" banner the moment this feature ships, even though a human DID
/// approve it (by running `insert_version`) before this UI existed.
///
/// # Errors
/// Returns an error if the manifest query or a preference read/write fails.
pub async fn list_connectors(db: &DbHandle) -> Result<Vec<ConnectorSummary>> {
    let rows = connectors::list_all(db).await?;
    let mut latest: HashMap<String, &ConnectorManifestRow> = HashMap::new();
    for row in &rows {
        latest
            .entry(row.connector_name.clone())
            .and_modify(|cur| {
                if row.created_at > cur.created_at {
                    *cur = row;
                }
            })
            .or_insert(row);
    }
    let mut entries: Vec<(String, &ConnectorManifestRow)> = latest.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut summaries = Vec::with_capacity(entries.len());
    for (_, row) in entries {
        summaries.push(build_summary(db, row).await?);
    }
    Ok(summaries)
}

async fn build_summary(db: &DbHandle, live: &ConnectorManifestRow) -> Result<ConnectorSummary> {
    let live_manifest = manifest::parse(&live.manifest_json).ok();
    let risk_tier = live_manifest.as_ref().map_or(RiskTier::IrreversibleWrite, connector_risk_tier);
    let cred_ref = live_manifest.as_ref().and_then(|m| m.auth.as_ref()).map(|a| a.cred_ref.clone());

    let reapproval = match live_manifest.as_ref() {
        Some(live_m) => resolve_reapproval(db, live, live_m).await?,
        None => None,
    };

    Ok(ConnectorSummary {
        id: live.id.clone(),
        connector_name: live.connector_name.clone(),
        version: live.version.clone(),
        status: live.status.clone(),
        base_url_host: base_url_host(&live.base_url),
        risk_tier: tier_label(risk_tier).to_string(),
        cred_ref,
        reapproval,
    })
}

/// Backfills the baseline on first-ever view (see [`list_connectors`]); otherwise computes
/// the whitelisted diff against the last-acknowledged version when they differ. Mirrors
/// `manifest::check_version`'s logic inline rather than calling it, since this call site
/// already holds the DB row `check_version` would otherwise need re-fetched.
async fn resolve_reapproval(
    db: &DbHandle,
    live: &ConnectorManifestRow,
    live_manifest: &Manifest,
) -> Result<Option<ReapprovalState>> {
    let pref_key = manifest::approved_version_pref_key(&live.connector_name);
    let approved_version = meta::get_preference(db, &pref_key).await?;

    let Some(approved_version) = approved_version else {
        meta::upsert_preference(db, &pref_key, &live.version, "connector_config_baseline").await?;
        return Ok(None);
    };
    if approved_version == live.version {
        return Ok(None);
    }

    let old_manifest = connectors::get_by_name_version(db, &live.connector_name, &approved_version)
        .await?
        .and_then(|row| manifest::parse(&row.manifest_json).ok());
    let diff = old_manifest
        .map(|old| manifest::manifest_diff(&old, live_manifest))
        .unwrap_or_default();
    Ok(Some(ReapprovalState { approved_version, live_version: live.version.clone(), diff }))
}

fn tier_rank(tier: RiskTier) -> u8 {
    match tier {
        RiskTier::Read => 0,
        RiskTier::ReversibleWrite => 1,
        RiskTier::IrreversibleWrite => 2,
        RiskTier::Blocked => 3,
    }
}

fn tier_label(tier: RiskTier) -> &'static str {
    match tier {
        RiskTier::Read => "Read",
        RiskTier::ReversibleWrite => "ReversibleWrite",
        RiskTier::IrreversibleWrite => "IrreversibleWrite",
        RiskTier::Blocked => "Blocked",
    }
}

/// The highest-blast-radius tier among a manifest's declared ops — fail-closed to
/// `IrreversibleWrite` for an ops-less manifest, matching `OpSpec::risk_tier`'s own
/// fail-closed contract for an unparseable/absent tier.
fn connector_risk_tier(m: &Manifest) -> RiskTier {
    m.ops
        .iter()
        .map(OpSpec::risk_tier)
        .max_by_key(|t| tier_rank(*t))
        .unwrap_or(RiskTier::IrreversibleWrite)
}

/// Host-only slice of a manifest's `base_url`, for the summary list — the path/query is
/// never needed for display. Manifest `base_url` is human-authored at approval time, not
/// attacker-controlled input, so this is a display helper, not a security boundary (C3's
/// pinned IP/CIDR gate is the real one, evaluated elsewhere against the full URL).
fn base_url_host(base_url: &str) -> String {
    let without_scheme = base_url.split_once("://").map_or(base_url, |(_, rest)| rest);
    without_scheme.split('/').next().unwrap_or(without_scheme).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_host_strips_scheme_and_path() {
        assert_eq!(base_url_host("https://erp.example.com/jsonrpc"), "erp.example.com");
        assert_eq!(base_url_host("http://192.168.1.5:8069"), "192.168.1.5:8069");
        assert_eq!(base_url_host("erp.example.com"), "erp.example.com");
    }

    #[test]
    fn tier_rank_orders_from_read_to_blocked() {
        assert!(tier_rank(RiskTier::Read) < tier_rank(RiskTier::ReversibleWrite));
        assert!(tier_rank(RiskTier::ReversibleWrite) < tier_rank(RiskTier::IrreversibleWrite));
        assert!(tier_rank(RiskTier::IrreversibleWrite) < tier_rank(RiskTier::Blocked));
    }
}
