//! Manifest version diffing — the whitelisted, re-approval-surfaced subset of what changed
//! between two manifest versions. Deliberately NOT a raw diff of `manifest_json` (that
//! document is connector-authored, semi-trusted at best); every field compared here is
//! explicitly whitelisted and every string run through `strip_tool_tags` before being
//! placed in the result, matching the C5 discipline the fault-string path uses elsewhere.
//! Field-level rendering/comparison lives in [`diff_helpers`], kept separate so this file
//! stays readable as "what gets compared" rather than "how each shape compares."
#[path = "diff_helpers.rs"]
mod diff_helpers;

use super::schema::Manifest;
use crate::connector::redact::strip_tool_tags;
use diff_helpers::{auth_fields, diff_cidrs, diff_field, diff_field_str, diff_protocol};
use serde::Serialize;

/// A per-op change between two manifest versions, whitelisted-field-only (m1). `op_name`
/// identifies which declared operation changed; the tier/compensability fields are `None`
/// when that field did not change for this op.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpDiff {
    pub op_name: String,
    /// `Some((old, new))` string tier labels when `risk_tier` changed for this op.
    pub risk_tier: Option<(String, String)>,
    /// `Some((old, new))` compensability strings when it changed for this op.
    pub compensability: Option<(String, String)>,
}

/// The whitelisted, structured diff between two manifest versions. Grew in v2 (m1) to cover
/// every `auth`/`protocol` field, PLUS `base_url`/`allowed_ip_cidrs` when `auth` is present
/// on either side (see [`manifest_diff`] docs) — once a manifest carries a credential, those
/// two fields decide where the secret is sent, so a change to them is blast-radius-relevant
/// in a way it is not for an auth-less manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ManifestDiff {
    /// Op names present in `new` but not `old`.
    pub added_ops: Vec<String>,
    /// Op names present in `old` but not `new`.
    pub removed_ops: Vec<String>,
    /// Ops present in both versions whose `risk_tier` and/or `compensability` changed.
    pub changed_ops: Vec<OpDiff>,
    pub auth_scheme: Option<(String, String)>,
    pub auth_cred_ref: Option<(String, String)>,
    pub auth_header_name: Option<(String, String)>,
    pub auth_param_name: Option<(String, String)>,
    pub protocol_endpoint_suffix: Option<(String, String)>,
    pub protocol_envelope: Option<(String, String)>,
    pub protocol_methods: Option<(String, String)>,
    pub protocol_fault_rules: Option<(String, String)>,
    pub protocol_readback: Option<(String, String)>,
    pub protocol_context: Option<(String, String)>,
    pub protocol_prevalidate: Option<(String, String)>,
    /// (M1) `Some` only when `auth` is present on `old` or `new` — see [`manifest_diff`].
    pub base_url: Option<(String, String)>,
    /// (M1) Order-insensitive: reordering the same CIDR set is not a change. Same `auth`
    /// gating as `base_url`.
    pub allowed_ip_cidrs: Option<(Vec<String>, Vec<String>)>,
}

impl ManifestDiff {
    /// `true` when nothing whitelisted changed between the two versions — the caller can
    /// skip forcing re-approval in that case. A `base_url`/`allowed_ip_cidrs`-only edit on
    /// an auth-LESS manifest is still out of scope (those already require a brand-new
    /// immutable row per the phase-13 trigger, and carry no secret whose destination could
    /// move); an auth-BEARING manifest's `base_url`/`allowed_ip_cidrs` edit is IN scope (M1)
    /// because it relocates where the credential is sent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added_ops.is_empty()
            && self.removed_ops.is_empty()
            && self.changed_ops.is_empty()
            && self.auth_scheme.is_none()
            && self.auth_cred_ref.is_none()
            && self.auth_header_name.is_none()
            && self.auth_param_name.is_none()
            && self.protocol_endpoint_suffix.is_none()
            && self.protocol_envelope.is_none()
            && self.protocol_methods.is_none()
            && self.protocol_fault_rules.is_none()
            && self.protocol_readback.is_none()
            && self.protocol_context.is_none()
            && self.protocol_prevalidate.is_none()
            && self.base_url.is_none()
            && self.allowed_ip_cidrs.is_none()
    }
}

/// Compare two manifest versions and return the whitelisted diff. Pure/offline — no remote
/// fetch, both manifests are already-stored rows the caller loaded (e.g. the
/// previously-approved version vs. the current live one).
///
/// (M1) `base_url`/`allowed_ip_cidrs` are compared ONLY when `old.auth` or `new.auth` is
/// `Some` — see [`ManifestDiff::is_empty`] for why the gate is on `auth`'s presence, not the
/// manifest version.
#[must_use]
pub fn manifest_diff(old: &Manifest, new: &Manifest) -> ManifestDiff {
    let mut diff = ManifestDiff::default();

    for new_op in &new.ops {
        match old.ops.iter().find(|o| o.name == new_op.name) {
            None => diff.added_ops.push(strip_tool_tags(&new_op.name)),
            Some(old_op) => {
                let risk_tier = diff_field(&old_op.risk_tier, &new_op.risk_tier);
                let compensability = diff_field(&old_op.compensability, &new_op.compensability);
                if risk_tier.is_some() || compensability.is_some() {
                    diff.changed_ops.push(OpDiff {
                        op_name: strip_tool_tags(&new_op.name),
                        risk_tier,
                        compensability,
                    });
                }
            }
        }
    }
    for old_op in &old.ops {
        if !new.ops.iter().any(|o| o.name == old_op.name) {
            diff.removed_ops.push(strip_tool_tags(&old_op.name));
        }
    }

    let (old_scheme, old_cred_ref, old_header, old_param) = auth_fields(&old.auth);
    let (new_scheme, new_cred_ref, new_header, new_param) = auth_fields(&new.auth);
    diff.auth_scheme = diff_field(&old_scheme, &new_scheme);
    diff.auth_cred_ref = diff_field(&old_cred_ref, &new_cred_ref);
    diff.auth_header_name = diff_field(&old_header, &new_header);
    diff.auth_param_name = diff_field(&old_param, &new_param);

    diff_protocol(&old.protocol, &new.protocol, &mut diff);

    // M1: base_url/allowed_ip_cidrs only become diff-relevant once a credential exists to
    // relocate. Gate on auth's presence on EITHER side so an auth being added/removed in
    // the same version bump still surfaces its own base_url/cidr state.
    if old.auth.is_some() || new.auth.is_some() {
        diff.base_url = diff_field_str(&old.base_url, &new.base_url);
        diff.allowed_ip_cidrs = diff_cidrs(&old.allowed_ip_cidrs, &new.allowed_ip_cidrs);
    }

    diff
}

/// The `kms_preferences` key holding the LAST version a human explicitly approved for
/// `connector_name`. Read/write by the approval-time caller (m1) — this module only
/// defines the naming convention; it does not perform the DB read itself, so it stays free
/// of any approval-flow orchestration.
#[must_use]
pub fn approved_version_pref_key(connector_name: &str) -> String {
    format!("connector.{connector_name}.approved_version")
}

/// The result of comparing a manifest's live version against the last-approved one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheck {
    /// No prior approval on record — first-time approval, nothing to diff against.
    NeverApproved,
    /// The live version matches the last-approved version; no re-approval needed.
    UpToDate,
    /// The live version differs from the last-approved one; carries the whitelisted diff
    /// against the approval-time caller's already-loaded `old` manifest, plus the raw
    /// version strings (own field, not connector-authored — no stripping needed) for
    /// display.
    Drifted {
        approved_version: String,
        live_version: String,
        /// Boxed: v2's auth/protocol fields grew `ManifestDiff` past clippy's
        /// large-enum-variant threshold relative to the other unit variants.
        diff: Box<ManifestDiff>,
    },
}

/// Compare `live`'s version against `approved_version` (the value read from
/// [`approved_version_pref_key`]) and, when they differ, compute the whitelisted diff
/// against `approved` (the manifest row for that approved version, if the caller has it
/// loaded — `None` when that row can no longer be found, e.g. a disabled/superseded row,
/// in which case the diff carries only the version strings, not stale field-level detail).
#[must_use]
pub fn check_version(
    approved_version: Option<&str>,
    approved: Option<&Manifest>,
    live: &Manifest,
) -> VersionCheck {
    let Some(approved_version) = approved_version else {
        return VersionCheck::NeverApproved;
    };
    if approved_version == live.version {
        return VersionCheck::UpToDate;
    }
    let diff = approved
        .map(|old| manifest_diff(old, live))
        .unwrap_or_default();
    VersionCheck::Drifted {
        approved_version: approved_version.to_string(),
        live_version: live.version.clone(),
        diff: Box::new(diff),
    }
}

#[cfg(test)]
#[path = "diff_tests.rs"]
mod tests;
