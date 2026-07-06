//! The per-deployment `connection` overlay (M4, locked decision) — everything the generic
//! `HttpExecutor` needs at call time that must NOT live inside the immutable, content-hashed
//! `Manifest`: a `base_url` OVERRIDE, `db`/`uid` (Odoo-shaped; `uid` in particular is the
//! product of an out-of-band `authenticate()` round-trip, not static config a human declares
//! up front), and an optional `cred_ref` override (the SAME approved manifest/protocol can be
//! deployed by two installs each pointing at their OWN credential name).
//!
//! Approving a manifest approves the DATA SHAPE (protocol/auth/ops); the overlay is
//! deployment config a human provisions SEPARATELY and can change freely without
//! re-approving anything — `haily_db::queries::connectors::content_hash` is computed from
//! `manifest_json` alone, which never contains these fields BY CONSTRUCTION (neither
//! `Manifest` nor `ProtocolSpec` declares a `db`/`uid` member), so an overlay edit can never
//! appear as a manifest change (M4's headline guarantee).
//!
//! Phase 3 defines the type + the read seam: [`crate::connector::HttpExecutor`] holds an
//! `Option<Self>` and consults it at call time via [`ConnectionOverlay::effective_base_url`] /
//! [`ConnectionOverlay::effective_cred_ref`] and the `{{db}}`/`{{uid}}` envelope tokens.
//! Phase 4a is the first REAL consumer: it authors the storage (a preference row, or a
//! sibling per-connector record — either way NOT the hashed manifest table) and rewires the
//! Odoo golden harness to populate `db`/`uid` from `HAILY_ODOO_DB`/`HAILY_ODOO_UID` via this
//! type instead of the (now-retired) `OdooExecutorConfig` fields.
use serde::{Deserialize, Serialize};

/// Per-install override values kept OUTSIDE the hashed manifest. Every field is optional —
/// `None` means "use the manifest's own value" (`base_url_override`) or "this
/// protocol/connector does not use this token" (`db`/`uid`). A `protocol.envelope` that
/// references `{{db}}`/`{{uid}}` with no overlay value supplied fails closed at call time (an
/// unresolvable substitution token) — never a guessed/default identity.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConnectionOverlay {
    /// Overrides `Manifest::base_url` for THIS install (e.g. a self-hosted instance's real
    /// address, vs. a placeholder in a shared/approved manifest). `None` = use the manifest.
    #[serde(default)]
    pub base_url_override: Option<String>,
    /// The `{{db}}` envelope token (Odoo's database-name positional).
    #[serde(default)]
    pub db: Option<String>,
    /// The `{{uid}}` envelope token — the product of an out-of-band `authenticate()` result,
    /// not a value a manifest author could declare ahead of time.
    #[serde(default)]
    pub uid: Option<i64>,
    /// Overrides `AuthSpec::cred_ref` for THIS install. `None` = use the manifest auth's own
    /// declared `cred_ref`.
    #[serde(default)]
    pub cred_ref_override: Option<String>,
}

impl ConnectionOverlay {
    /// The base_url to actually call: the overlay override, else the manifest's own.
    #[must_use]
    pub fn effective_base_url<'a>(&'a self, manifest_base_url: &'a str) -> &'a str {
        self.base_url_override.as_deref().unwrap_or(manifest_base_url)
    }

    /// The credential reference NAME to resolve: the overlay override, else the manifest
    /// auth's own `cred_ref`.
    #[must_use]
    pub fn effective_cred_ref<'a>(&'a self, manifest_cred_ref: &'a str) -> &'a str {
        self.cred_ref_override.as_deref().unwrap_or(manifest_cred_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_values_fall_back_to_manifest_when_overlay_omits_them() {
        let overlay = ConnectionOverlay::default();
        assert_eq!(overlay.effective_base_url("https://m.example.com"), "https://m.example.com");
        assert_eq!(overlay.effective_cred_ref("connector.x.api_key"), "connector.x.api_key");
    }

    #[test]
    fn overlay_overrides_take_priority_over_the_manifest() {
        let overlay = ConnectionOverlay {
            base_url_override: Some("https://real.example.com".into()),
            cred_ref_override: Some("connector.x.other_key".into()),
            ..Default::default()
        };
        assert_eq!(
            overlay.effective_base_url("https://placeholder.example.com"),
            "https://real.example.com"
        );
        assert_eq!(overlay.effective_cred_ref("connector.x.api_key"), "connector.x.other_key");
    }

    #[test]
    fn overlay_round_trips_through_json_for_future_persistence() {
        let overlay = ConnectionOverlay {
            base_url_override: None,
            db: Some("haily_ci".into()),
            uid: Some(2),
            cred_ref_override: None,
        };
        let s = serde_json::to_string(&overlay).unwrap();
        let back: ConnectionOverlay = serde_json::from_str(&s).unwrap();
        assert_eq!(back, overlay);
    }
}
