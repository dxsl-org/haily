//! Mobile-server config loader (Mobile Thin-Client plan phase 2a) — mirrors
//! `config::load_llm_config`'s shape exactly: read `kms_preferences`, falling back to
//! `MobileServerConfig::default()` (server DISABLED, tailnet+loopback-only bind, biometric-
//! required approval policy) for anything unset. Never fails — a missing/malformed preference
//! simply leaves that field at its safe default.
use haily_db::{queries::meta, DbHandle};
use haily_io::mobile::MobileServerConfig;
use haily_types::MobileApprovalPolicy;

/// `"true"`/`"1"` is on, anything else (including absent) is off — the safe reading for every
/// boolean preference below.
fn is_truthy(v: &str) -> bool {
    v == "true" || v == "1"
}

pub async fn load_mobile_config(db: &DbHandle) -> MobileServerConfig {
    let mut cfg = MobileServerConfig::default();

    if let Ok(Some(v)) = meta::get_preference(db, "mobile.enabled").await {
        cfg.enabled = is_truthy(&v);
    }
    if let Ok(Some(v)) = meta::get_preference(db, "mobile.port").await {
        if let Ok(port) = v.parse::<u16>() {
            cfg.port = port;
        }
    }
    if let Ok(Some(v)) = meta::get_preference(db, "mobile.lan_opt_in").await {
        cfg.lan_opt_in = is_truthy(&v);
    }
    if let Ok(Some(v)) = meta::get_preference(db, "mobile.approval_policy").await {
        cfg.approval_policy = match v.as_str() {
            "allow" => MobileApprovalPolicy::Allow,
            "deny-irreversible" => MobileApprovalPolicy::DenyIrreversible,
            // Any other value (including a typo) falls back to the safest option, never
            // silently to `Allow` — mirrors `DepthMode::from_label`'s "never escalate on
            // garbage input" convention.
            _ => MobileApprovalPolicy::BiometricRequired,
        };
    }
    if let Ok(Some(v)) = meta::get_preference(db, "mobile.inbound_rate_limit_per_minute").await {
        if let Ok(n) = v.parse::<u32>() {
            cfg.inbound_rate_limit_per_minute = n;
        }
    }
    if let Ok(Some(v)) = meta::get_preference(db, "mobile.deny_remote_deep").await {
        cfg.deny_remote_deep = is_truthy(&v);
    }

    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> DbHandle {
        let dir = tempfile::tempdir().unwrap();
        DbHandle::init(&dir.path().join("t.db")).await.unwrap()
    }

    #[tokio::test]
    async fn unset_preferences_yield_the_safe_default_config() {
        let db = test_db().await;
        let cfg = load_mobile_config(&db).await;
        assert!(!cfg.enabled);
        assert!(!cfg.lan_opt_in);
        assert_eq!(cfg.approval_policy, MobileApprovalPolicy::BiometricRequired);
    }

    #[tokio::test]
    async fn preferences_override_the_defaults() {
        let db = test_db().await;
        meta::upsert_preference(&db, "mobile.enabled", "true", "test")
            .await
            .unwrap();
        meta::upsert_preference(&db, "mobile.lan_opt_in", "true", "test")
            .await
            .unwrap();
        meta::upsert_preference(&db, "mobile.port", "9999", "test")
            .await
            .unwrap();
        meta::upsert_preference(&db, "mobile.approval_policy", "deny-irreversible", "test")
            .await
            .unwrap();
        meta::upsert_preference(&db, "mobile.inbound_rate_limit_per_minute", "10", "test")
            .await
            .unwrap();
        meta::upsert_preference(&db, "mobile.deny_remote_deep", "false", "test")
            .await
            .unwrap();

        let cfg = load_mobile_config(&db).await;
        assert!(cfg.enabled);
        assert!(cfg.lan_opt_in);
        assert_eq!(cfg.port, 9999);
        assert_eq!(cfg.approval_policy, MobileApprovalPolicy::DenyIrreversible);
        assert_eq!(cfg.inbound_rate_limit_per_minute, 10);
        assert!(!cfg.deny_remote_deep);
    }

    /// A malformed policy value must fall back to the SAFEST option, never `Allow`.
    #[tokio::test]
    async fn malformed_approval_policy_falls_back_to_biometric_required() {
        let db = test_db().await;
        meta::upsert_preference(&db, "mobile.approval_policy", "not-a-real-policy", "test")
            .await
            .unwrap();
        let cfg = load_mobile_config(&db).await;
        assert_eq!(cfg.approval_policy, MobileApprovalPolicy::BiometricRequired);
    }
}
