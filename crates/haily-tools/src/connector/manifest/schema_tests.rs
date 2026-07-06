//! Tests for `schema.rs`, split out to keep the schema definitions themselves scannable.
use super::*;
use serde_json::json;

fn op(risk_tier: Option<&str>) -> OpSpec {
    OpSpec {
        name: "odoo_write".into(),
        model: Some("res.partner".into()),
        method: Some("write".into()),
        risk_tier: risk_tier.map(str::to_string),
        compensability: Some("compensatable".into()),
        compensation: Some(json!({"op": "write"})),
        correlation_field: Some("ref".into()),
    }
}

#[test]
fn risk_tier_parses_declared_values() {
    assert_eq!(op(Some("Read")).risk_tier(), RiskTier::Read);
    assert_eq!(
        op(Some("ReversibleWrite")).risk_tier(),
        RiskTier::ReversibleWrite
    );
    assert_eq!(
        op(Some("IrreversibleWrite")).risk_tier(),
        RiskTier::IrreversibleWrite
    );
    assert_eq!(op(Some("Blocked")).risk_tier(), RiskTier::Blocked);
}

#[test]
fn risk_tier_fail_closes_on_absent_or_unknown() {
    // Absent tier → IrreversibleWrite (unresolvable = worst case).
    assert_eq!(op(None).risk_tier(), RiskTier::IrreversibleWrite);
    // Unrecognized string → IrreversibleWrite (malformed = worst case).
    assert_eq!(op(Some("Cheap")).risk_tier(), RiskTier::IrreversibleWrite);
    assert_eq!(op(Some("")).risk_tier(), RiskTier::IrreversibleWrite);
}

#[test]
fn parse_valid_manifest_with_ops() {
    let json = r#"{
        "connector_name": "odoo",
        "version": "1",
        "base_url": "https://erp.example.com",
        "allowed_ip_cidrs": ["93.184.216.34/32"],
        "ops": [
            {"name":"odoo_contact_create","model":"res.partner","method":"create",
             "risk_tier":"IrreversibleWrite","compensability":"compensatable",
             "compensation":{"op":"unlink"}}
        ]
    }"#;
    let m = parse(json).unwrap();
    assert_eq!(m.connector_name, "odoo");
    assert_eq!(m.allowed_ip_cidrs, vec!["93.184.216.34/32"]);
    assert_eq!(m.ops.len(), 1);
    assert_eq!(m.ops[0].name, "odoo_contact_create");
    assert!(m.ops[0].is_create());
    assert!(m.auth.is_none());
    assert!(m.protocol.is_none());
}

#[test]
fn parse_rejects_malformed_json() {
    assert!(parse("not json {{{").is_err());
    assert!(parse(r#"{"version":"1"}"#).is_err()); // missing required fields
}

#[test]
fn compensability_defaults_to_final() {
    let mut o = op(None);
    o.compensability = None;
    assert_eq!(o.compensability_str(), "final");
}

fn v2_manifest_json(auth_json: &str) -> String {
    format!(
        r#"{{"connector_name":"stripe","version":"1","base_url":"https://api.stripe.com",
            "allowed_ip_cidrs":[],"ops":[],
            "auth":{auth_json},
            "protocol":{{
                "endpoint_suffix":"/v1",
                "envelope":{{"body":"{{{{args}}}}"}},
                "methods":[{{"method":"create","arg_template":["{{{{vals}}}}"]}}],
                "fault_rules":[{{"match_field":"status","match_value":"404","normalized":"MissingError"}}],
                "readback":{{"locate_by":"id","active_test":false,"unwrap_first":true}},
                "context":{{"lang":"vi_VN"}},
                "prevalidate":[{{"model":"customer","required_fields":["email"]}}]
            }}
        }}"#
    )
}

#[test]
fn parse_v2_manifest_with_bearer_auth_and_protocol() {
    let json = v2_manifest_json(r#"{"scheme":"bearer","cred_ref":"connector.stripe.api_key"}"#);
    let m = parse(&json).unwrap();
    let auth = m.auth.expect("auth present");
    assert_eq!(auth.scheme, "bearer");
    assert_eq!(auth.resolve().unwrap(), ResolvedAuthScheme::Bearer);
    let protocol = m.protocol.expect("protocol present");
    assert_eq!(protocol.endpoint_suffix.as_deref(), Some("/v1"));
    assert_eq!(protocol.methods.len(), 1);
    assert_eq!(protocol.fault_rules.len(), 1);
    assert_eq!(protocol.prevalidate.len(), 1);
    assert!(protocol.readback.unwrap().unwrap_first);
}

#[test]
fn parse_rejects_unknown_auth_scheme() {
    let json = v2_manifest_json(r#"{"scheme":"oauth2","cred_ref":"connector.stripe.api_key"}"#);
    let err = parse(&json).unwrap_err();
    assert!(err.to_string().contains("unknown auth scheme"), "{err}");
}

#[test]
fn parse_rejects_header_scheme_missing_header_name() {
    let json = v2_manifest_json(r#"{"scheme":"header","cred_ref":"connector.stripe.api_key"}"#);
    let err = parse(&json).unwrap_err();
    assert!(err.to_string().contains("header_name"), "{err}");
}

#[test]
fn parse_rejects_query_param_scheme_missing_param_name() {
    let json = v2_manifest_json(r#"{"scheme":"query-param","cred_ref":"connector.stripe.api_key"}"#);
    let err = parse(&json).unwrap_err();
    assert!(err.to_string().contains("param_name"), "{err}");
}

#[test]
fn parse_accepts_header_and_query_param_schemes_with_name_present() {
    let header_json = v2_manifest_json(
        r#"{"scheme":"header","cred_ref":"connector.stripe.api_key","header_name":"X-API-Key"}"#,
    );
    let m = parse(&header_json).unwrap();
    assert_eq!(
        m.auth.unwrap().resolve().unwrap(),
        ResolvedAuthScheme::Header("X-API-Key".to_string())
    );

    let qp_json = v2_manifest_json(
        r#"{"scheme":"query-param","cred_ref":"connector.stripe.api_key","param_name":"api_key"}"#,
    );
    let m = parse(&qp_json).unwrap();
    assert_eq!(
        m.auth.unwrap().resolve().unwrap(),
        ResolvedAuthScheme::QueryParam("api_key".to_string())
    );
}
