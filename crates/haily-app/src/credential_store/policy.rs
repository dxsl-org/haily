//! Fallback-behavior policy for [`super::CredentialStore`] — split out of `mod.rs` as a
//! self-contained unit (the toggles + their two constructors, no keyring/DB dependency).

/// Policy toggles controlling fallback behavior. All three default to the safe posture
/// for an interactive single-user desktop: attempt the keyring, allow read-fallback,
/// refuse write-fallback. `--headless` overrides `attempt_keyring` to `false` at startup
/// (M5a) before any [`super::CredentialStore`] is constructed.
#[derive(Debug, Clone, Copy)]
pub struct CredentialPolicy {
    /// `false` in `--headless` (Session-0/no-D-Bus is structurally unreliable) — skip the
    /// keyring entirely and go straight to the DB-read path.
    pub attempt_keyring: bool,
    /// `true` by default (M5b safe direction): a keyring READ error may fall back to
    /// reading the plaintext `kms_preferences` value.
    pub allow_read_fallback: bool,
    /// `false` by default (M5b dangerous direction): a keyring WRITE error does NOT
    /// silently fall back to writing plaintext. Must be explicitly opted into.
    pub allow_write_plaintext: bool,
}

impl Default for CredentialPolicy {
    fn default() -> Self {
        Self {
            attempt_keyring: true,
            allow_read_fallback: true,
            allow_write_plaintext: false,
        }
    }
}

impl CredentialPolicy {
    /// The headless/Session-0 policy (M5a): never attempt the keyring, everything else at
    /// its safe default.
    #[must_use]
    pub fn headless() -> Self {
        Self {
            attempt_keyring: false,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_interactive_safe_posture() {
        let p = CredentialPolicy::default();
        assert!(p.attempt_keyring);
        assert!(p.allow_read_fallback);
        assert!(!p.allow_write_plaintext);
    }

    #[test]
    fn headless_policy_never_attempts_keyring_but_keeps_other_defaults() {
        let p = CredentialPolicy::headless();
        assert!(!p.attempt_keyring);
        assert!(p.allow_read_fallback);
        assert!(!p.allow_write_plaintext);
    }
}
