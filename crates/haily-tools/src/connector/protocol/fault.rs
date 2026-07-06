//! Declarative fault classification driven by `protocol.fault_rules`, reusing
//! `odoo_fault::extract_fault` so the generic path reads the SAME JSON-RPC error shape
//! `OdooExecutor` does (M7: classify strictly on a STRUCTURED field, never the human
//! message). Mirrors `odoo_executor::fault_class_token`'s fail-closed default.
use crate::connector::manifest::FaultRule;
use crate::connector::odoo_fault::OdooFault;

/// The closed set of structured fields a `FaultRule.match_field` may name — no dotted paths,
/// no regex, string equality only (the schema's own contract): `"name"` (the fault's
/// `data.name`, e.g. `odoo.exceptions.AccessError`), `"code"` (the JSON-RPC `error.code`), or
/// `"status"` (the outer HTTP status, for a connector that signals faults via status alone
/// rather than an embedded `error` object). Any other string never matches — fail-closed, not
/// a panic.
const FIELD_NAME: &str = "name";
const FIELD_CODE: &str = "code";
const FIELD_STATUS: &str = "status";

/// Default normalized token for a fault matched by NO declared rule — fail-closed (never
/// guessed from the human message), mirroring `FaultClass::Unknown`'s `"UnknownError"` token.
pub const UNKNOWN_TOKEN: &str = "UnknownError";

/// Classify `fault` (plus the outer HTTP `status`, when known) against `rules` in order,
/// returning the first match's `normalized` token, or [`UNKNOWN_TOKEN`] when none match.
#[must_use]
pub fn classify_fault(rules: &[FaultRule], fault: &OdooFault, status: Option<u16>) -> String {
    for rule in rules {
        let matched = match rule.match_field.as_str() {
            FIELD_NAME => fault.name.as_deref() == Some(rule.match_value.as_str()),
            FIELD_CODE => fault.code.as_deref() == Some(rule.match_value.as_str()),
            FIELD_STATUS => status.map(|s| s.to_string()).as_deref() == Some(rule.match_value.as_str()),
            _ => false,
        };
        if matched {
            return rule.normalized.clone();
        }
    }
    UNKNOWN_TOKEN.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Vec<FaultRule> {
        vec![
            FaultRule {
                match_field: "name".into(),
                match_value: "odoo.exceptions.AccessError".into(),
                normalized: "AccessError".into(),
            },
            FaultRule {
                match_field: "name".into(),
                match_value: "odoo.exceptions.ValidationError".into(),
                normalized: "ValidationError".into(),
            },
            FaultRule {
                match_field: "name".into(),
                match_value: "odoo.exceptions.MissingError".into(),
                normalized: "MissingError".into(),
            },
        ]
    }

    fn fault(name: &str) -> OdooFault {
        OdooFault { code: Some("200".into()), name: Some(name.into()), fault_string: "human text".into() }
    }

    #[test]
    fn classifies_the_three_recognized_odoo_classes() {
        assert_eq!(classify_fault(&rules(), &fault("odoo.exceptions.AccessError"), None), "AccessError");
        assert_eq!(classify_fault(&rules(), &fault("odoo.exceptions.ValidationError"), None), "ValidationError");
        assert_eq!(classify_fault(&rules(), &fault("odoo.exceptions.MissingError"), None), "MissingError");
    }

    #[test]
    fn unrecognized_or_absent_name_fails_closed_to_unknown() {
        assert_eq!(classify_fault(&rules(), &fault("odoo.exceptions.SomeNewException"), None), UNKNOWN_TOKEN);
        let no_name = OdooFault { code: Some("200".into()), name: None, fault_string: "text".into() };
        assert_eq!(classify_fault(&rules(), &no_name, None), UNKNOWN_TOKEN);
    }

    #[test]
    fn status_keyed_rule_classifies_a_pure_http_status_fault() {
        let status_rules = vec![FaultRule {
            match_field: "status".into(),
            match_value: "403".into(),
            normalized: "AccessError".into(),
        }];
        let f = OdooFault { code: Some("403".into()), name: None, fault_string: "forbidden".into() };
        assert_eq!(classify_fault(&status_rules, &f, Some(403)), "AccessError");
        assert_eq!(classify_fault(&status_rules, &f, Some(500)), UNKNOWN_TOKEN);
    }

    #[test]
    fn an_unrecognized_match_field_never_matches() {
        let weird_rules = vec![FaultRule {
            match_field: "totally_unsupported".into(),
            match_value: "whatever".into(),
            normalized: "ShouldNeverHit".into(),
        }];
        assert_eq!(classify_fault(&weird_rules, &fault("odoo.exceptions.AccessError"), None), UNKNOWN_TOKEN);
    }
}
