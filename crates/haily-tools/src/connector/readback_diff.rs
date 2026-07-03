//! Read-back field diff — the single source of truth for "does the record Odoo READ back
//! match what we SENT?". Used by both the post-write verify (`HttpConnectorTool`) and the
//! crash-recovery sweep (`reconcile`) so the two paths can never diverge.
//!
//! ## Why a normalizer, not a raw `==`
//!
//! Odoo's READ representation of a field frequently differs in FORMAT — not VALUE — from
//! what a client SENDS. A raw `sent == read` comparison then false-flags a correctly-written
//! record as `mismatch`, which (for a create, having no clean pre_state) causes `journal_undo`
//! to REFUSE the undo. The classic case: a `many2one` is SENT as a scalar id (`88`) but READ
//! back as `[88, "Display Name"]`.
//!
//! Read-back exists to catch a SILENTLY-WRONG write (the server stored a different value than
//! we asked for). So the normalization below is strictly about REPRESENTATION, never TOLERANCE:
//! after normalizing both sides to a canonical shape, a genuine scalar value difference
//! (`name`, `email`, `summary`) still compares unequal and still marks `mismatch`.
//!
//! Each rule infers the representation from the VALUE SHAPE alone — no extra `fields_get` RPC:
//!   - `[int, str]`  → a many2one read form → compare the id only.
//!   - `false`       → an unset/empty field → equal to an absent/empty sent value.
//!   - `[int, ...]`  → a list-of-ids relational read form → compare as an unordered id SET.
//!   - a number      → normalize int/float representation (`88` vs `88.0`) + numeric strings.
//!   - anything else → a scalar → compared strictly (this is the value-diff we MUST preserve).
use serde_json::Value;

/// True when every field present in the SENT `values`/params matches the value Odoo READ
/// back, after per-field representation normalization. A field the server ADDED (e.g.
/// `create_date`) is ignored — only what the client asked to write is verified. Redacted
/// credential markers are skipped (they are not record fields).
///
/// `expected` is the SENT side (already unwrapped to the `values` object by the caller);
/// `body` is the record Odoo READ back. Returns `false` on the FIRST genuine mismatch.
pub fn request_fields_match(expected: &Value, body: &Value) -> bool {
    let expected_map = match expected.as_object() {
        Some(m) => m,
        // Nothing structured to diff → do not claim a mismatch (fail-open on shape, not value).
        None => return true,
    };
    for (field, sent) in expected_map {
        // A redacted credential reference is not a record field — never diffed.
        if sent.as_str().is_some_and(|s| s.starts_with("<redacted:")) {
            continue;
        }
        let read = body.get(field);
        if !field_matches(sent, read) {
            return false;
        }
    }
    true
}

/// Compare one SENT field value against its READ-back value (`None` when the read body has
/// no such key). The comparison normalizes Odoo's read representation to the sent shape so a
/// format-only difference is NOT a mismatch, while a real value difference still is.
fn field_matches(sent: &Value, read: Option<&Value>) -> bool {
    match read {
        // The field is absent from the read body. That is a match ONLY when the sent value is
        // itself "unset" (null / empty string / explicit `false`) — sending nothing and reading
        // nothing agree. A concrete sent value with no read-back is a genuine mismatch.
        None => is_unset(sent),
        Some(read) => values_equivalent(sent, read),
    }
}

/// Canonical equivalence of a SENT value and a READ value, ignoring representation.
fn values_equivalent(sent: &Value, read: &Value) -> bool {
    // unset ↔ unset: Odoo returns `false` for an empty scalar/relational field; a client may
    // send `false`, `null`, `""`, or omit it. All of these agree with a read `false`/null/"".
    if is_unset(sent) && is_unset(read) {
        return true;
    }
    // many2one: read form is `[id, "display_name"]`, sent is the scalar id. Compare ids.
    if let (Some(read_id), Some(sent_id)) = (many2one_id(read), scalar_id(sent)) {
        return read_id == sent_id;
    }
    // list-of-ids relational (many2many/one2many): read form is `[id, id, ...]`. Compare as an
    // unordered id SET against the ids the sent side implies. If the sent side is a Command-tuple
    // list whose target id-set cannot be determined unambiguously, treat the relational field as
    // NOT STRICTLY DIFFABLE and EXCLUDE it (match) rather than false-flag — never for a scalar.
    if let Some(read_ids) = id_list(read) {
        return match command_target_ids(sent) {
            Some(sent_ids) => set_eq(&read_ids, &sent_ids),
            // Ambiguous relational write (e.g. a partial LINK/UNLINK/UPDATE command) — cannot be
            // strictly diffed from the read snapshot alone, so do not claim a mismatch. Logged so
            // an operator can see WHY a relational field was not verified.
            None => {
                tracing::debug!(
                    sent = %sent,
                    "readback_diff: relational field not strictly diffable (ambiguous command tuples) — excluded from mismatch decision"
                );
                true
            }
        };
    }
    // numbers: normalize int vs float representation (`88` vs `88.0`) and numeric-string vs
    // number (Odoo may coerce). A NaN never equals anything (fails closed → mismatch).
    if let (Some(a), Some(b)) = (as_number(sent), as_number(read)) {
        return a == b;
    }
    // scalar value: strict equality. THIS is the silently-wrong-write check — a wrong
    // `name`/`email`/`summary` must still mismatch, so it is deliberately NOT loosened.
    sent == read
}

/// True when a value represents an unset/empty field: `null`, `false`, or the empty string.
/// Odoo reads an empty scalar/relational field back as `false`; clients express "unset" as
/// any of these — they are equivalent for diff purposes.
fn is_unset(v: &Value) -> bool {
    v.is_null() || v == &Value::Bool(false) || v.as_str() == Some("")
}

/// If `v` is a many2one READ form `[id, "display_name"]` (a 2-element array whose first
/// element is an integer), return the id. `false`/scalar/other shapes return `None`.
fn many2one_id(v: &Value) -> Option<i64> {
    let arr = v.as_array()?;
    if arr.len() == 2 && arr[1].is_string() {
        return arr[0].as_i64();
    }
    None
}

/// The scalar id a many2one is SENT as (an integer, or an integer-valued numeric string).
fn scalar_id(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
}

/// If `v` is a list-of-ids relational READ form (`[id, id, ...]`, every element an integer),
/// return the ids. An empty array counts (an empty relation). A `[int, str]` shape is a
/// many2one, NOT a list-of-ids, and is handled earlier — so exclude the 2-element-with-string
/// case by requiring EVERY element to be an integer.
fn id_list(v: &Value) -> Option<Vec<i64>> {
    let arr = v.as_array()?;
    let ids: Option<Vec<i64>> = arr.iter().map(Value::as_i64).collect();
    ids
}

/// The target id-set implied by a SENT relational write. Odoo x2many writes are Command
/// tuples `[code, id, values]`. This resolves the FINAL membership ONLY for the two commands
/// that express a complete set: `SET (6, 0, [ids])` and a raw list of scalar ids (treated as a
/// full set). Partial/mutating commands (LINK/UNLINK/CREATE/DELETE/UPDATE/CLEAR) cannot be
/// diffed against a read snapshot without the prior state, so they return `None` (ambiguous →
/// field excluded, never false-flagged). Codes mirror [`super::odoo_executor::command`].
fn command_target_ids(sent: &Value) -> Option<Vec<i64>> {
    let arr = sent.as_array()?;
    // A bare list of scalar ids `[1, 2, 3]` = the full membership.
    if let Some(ids) = id_list(sent) {
        return Some(ids);
    }
    // A single `SET` command `[[6, 0, [ids]]]` replaces the whole set → its ids ARE the target.
    if arr.len() == 1 {
        if let Some(cmd) = arr[0].as_array() {
            // command::SET == 6 — the only command carrying a complete, order-free id set.
            if cmd.len() == 3 && cmd[0].as_i64() == Some(6) {
                return id_list(&cmd[2]);
            }
        }
    }
    // Any other command shape is a partial mutation — ambiguous without prior state.
    None
}

/// Parse a JSON value as an f64 for numeric comparison: a JSON number, or a numeric string
/// Odoo may coerce (`"88"` / `"88.0"`). Non-numeric → `None` (falls through to strict compare).
fn as_number(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
}

/// Unordered set equality of two id vectors (relational membership ignores order + duplicates).
fn set_eq(a: &[i64], b: &[i64]) -> bool {
    let sa: std::collections::BTreeSet<i64> = a.iter().copied().collect();
    let sb: std::collections::BTreeSet<i64> = b.iter().copied().collect();
    sa == sb
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn many2one_read_form_matches_scalar_id_sent() {
        // The live bug: mail.activity create sends `res_model_id: 88` (scalar many2one), Odoo
        // reads it back as `[88, "Contact"]`. That is a FORMAT difference, not a value one.
        assert!(field_matches(&json!(88), Some(&json!([88, "Contact"]))));
        assert!(request_fields_match(
            &json!({"res_model_id": 88, "summary": "Ghost"}),
            &json!({"res_model_id": [88, "Contact"], "summary": "Ghost", "create_date": "2026-07-03"})
        ));
        // A WRONG many2one id (write landed on a different record) must STILL mismatch.
        assert!(!field_matches(&json!(88), Some(&json!([99, "Other"]))));
    }

    #[test]
    fn odoo_false_matches_absent_or_empty_sent() {
        // Odoo returns `false` for an empty field; sending null/""/false/omitted all agree.
        assert!(field_matches(&json!(false), Some(&json!(false))));
        assert!(field_matches(&Value::Null, Some(&json!(false))));
        assert!(field_matches(&json!(""), Some(&json!(false))));
        // Sent something, read `false` → the write did NOT land → mismatch.
        assert!(!field_matches(&json!("Alice"), Some(&json!(false))));
        // Sent unset, field absent from read body → match (nothing sent, nothing read).
        assert!(field_matches(&json!(false), None));
        assert!(field_matches(&Value::Null, None));
        // Sent a concrete value, field ABSENT from read → mismatch (write not verifiable).
        assert!(!field_matches(&json!("Alice"), None));
    }

    #[test]
    fn genuine_scalar_value_difference_still_mismatches() {
        // Representation normalization must NOT weaken the silently-wrong-write check.
        assert!(!field_matches(&json!("Alice"), Some(&json!("Bob"))));
        assert!(!request_fields_match(
            &json!({"name": "Alice", "email": "a@b.c"}),
            &json!({"name": "Alice", "email": "wrong@x.y"})
        ));
        // Exact scalar match still matches.
        assert!(request_fields_match(
            &json!({"name": "Alice"}),
            &json!({"name": "Alice", "create_date": "2026-07-03"})
        ));
    }

    #[test]
    fn id_set_relational_compares_by_unordered_set() {
        // A many2many/one2many reads back as `[id, id, ...]`; sent as a full id set or a SET
        // command. Membership is order-free.
        assert!(field_matches(&json!([1, 2, 3]), Some(&json!([3, 1, 2]))));
        // SET command `(6, 0, [ids])` expresses the complete membership.
        assert!(field_matches(
            &json!([[6, 0, [1, 2, 3]]]),
            Some(&json!([2, 3, 1]))
        ));
        // A DIFFERENT membership set must mismatch.
        assert!(!field_matches(&json!([1, 2, 3]), Some(&json!([1, 2]))));
        // A partial/ambiguous command (LINK) cannot be strictly diffed → excluded (match), so a
        // relational field is never false-flagged — but this must NEVER apply to a scalar.
        assert!(field_matches(&json!([[4, 5, 0]]), Some(&json!([1, 2, 5]))));
    }

    #[test]
    fn number_representation_int_float_and_string() {
        // Odoo may coerce int↔float or number↔numeric-string; representation-only diffs match.
        assert!(field_matches(&json!(88), Some(&json!(88.0))));
        assert!(field_matches(&json!("88"), Some(&json!(88))));
        assert!(field_matches(&json!(1.5), Some(&json!("1.5"))));
        // A genuinely different number still mismatches.
        assert!(!field_matches(&json!(88), Some(&json!(89))));
    }
}
