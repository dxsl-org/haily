//! DB-row marker conventions used by [`super::CredentialStore`]'s migration (M5c) and
//! read-fallback paths. Split out of `mod.rs` purely for file-size hygiene — these are
//! pure string helpers with no keyring/DB dependency of their own.

/// Preference flag the GUI reads on open to show a persisted fallback warning (M5a/M5b).
/// A flag row, not a log line — surviving state that outlives the log stream so a user who
/// wasn't watching the console at boot still sees the warning next time they open the app.
pub const FALLBACK_WARNING_PREF: &str = "credential.fallback_active";

/// Marker prefix written over a `kms_preferences` row once its secret has been migrated
/// into the keyring — distinguishes "already migrated" (no-op on next boot) from "still
/// holds the raw secret" (needs migration). Never a valid secret value itself.
pub const KEYRING_MARKER_PREFIX: &str = "keyring:";

/// `true` when `value` is the migration marker written over a migrated secret's DB row,
/// not a raw secret — used both to skip a re-migration and to make sure the DB
/// read-fallback path never returns a marker string as if it were a real secret.
#[must_use]
pub fn is_keyring_marker(value: &str) -> bool {
    value.starts_with(KEYRING_MARKER_PREFIX)
}

pub fn keyring_marker(cred_ref: &str) -> String {
    format!("{KEYRING_MARKER_PREFIX}{cred_ref}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_detection_round_trips() {
        let m = keyring_marker("connector.odoo.api_key");
        assert!(is_keyring_marker(&m));
        assert!(!is_keyring_marker("sk-a-real-secret-value"));
        assert!(!is_keyring_marker(""));
    }
}
