//! Cookie/session mapping (Phase 13) — the drop-in port of haily.go's `browser_session` cookie
//! handling. The `browser_session` tool (list/export/import/clear) lives in `tools.rs`; this
//! module holds the feature-INDEPENDENT `SameSite` mapping the import path relies on.
//!
//! Why an explicit mapping: CDP defaults an omitted `sameSite` to `Lax`, which breaks Facebook
//! and Google session cookies that rely on `SameSite=None` or unspecified semantics. The import
//! path MUST map the attribute explicitly (or leave it unset), never silently default it.

/// Cookie `SameSite` attribute, mirroring the CDP `Network.CookieSameSite` enum but defined
/// locally so this mapping is testable without the `browser` feature (chromiumoxide's enum is
/// only available under the feature). The `browser`-gated import path translates this into
/// chromiumoxide's `CookieSameSite`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

/// Map a cookie's `sameSite` string to [`SameSite`]. Returns `None` for an absent/empty/unknown
/// value — the import path then leaves the attribute UNSET rather than defaulting it to `Lax`
/// (which would break `SameSite=None` session cookies). Case-sensitive on the canonical
/// spellings, matching the prior Go switch.
pub fn map_same_site(raw: &str) -> Option<SameSite> {
    match raw {
        "Strict" => Some(SameSite::Strict),
        "Lax" => Some(SameSite::Lax),
        "None" => Some(SameSite::None),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_canonical_same_site_values() {
        assert_eq!(map_same_site("Strict"), Some(SameSite::Strict));
        assert_eq!(map_same_site("Lax"), Some(SameSite::Lax));
        assert_eq!(map_same_site("None"), Some(SameSite::None));
    }

    #[test]
    fn absent_or_unknown_stays_unset_not_lax() {
        // The load-bearing invariant: an omitted/unknown attribute must NOT silently become Lax,
        // or Facebook/Google `SameSite=None` cookies break.
        assert_eq!(map_same_site(""), None);
        assert_eq!(map_same_site("bogus"), None);
        assert_eq!(map_same_site("strict"), None); // case-sensitive on the canonical spelling
    }
}

// The live `browser_session` tool (CDP cookie list/export/import/clear) is defined in `tools.rs`
// behind the `browser` feature.
