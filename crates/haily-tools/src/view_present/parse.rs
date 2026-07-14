//! `present_view` args → [`DataView`] — strict parse, then ONE bounded repair pass for the
//! common weak-model deviations (mirrors `plan_pipeline::draft::{parse_and_repair,
//! draft_from_args}`'s tolerant-parse precedent). Never panics: every failure path returns a
//! clean `Err` that becomes the tool's (failed) result text, fed back to the model.

use anyhow::{bail, Context, Result};
use haily_types::{DataView, FieldDef, FieldType, ProjectionKind, ProjectionSpec, ViewProvenance};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

/// Intermediate shape for one `schema[]` entry. `writable` is intentionally NOT accepted from
/// the model — Phase A always forces it to `false` (see `RiskTier`/`DataView` docs: an
/// `LlmProjected` view is never form-capable) — so a model claiming a field writable is simply
/// ignored rather than trusted.
#[derive(Deserialize)]
struct ArgsFieldDef {
    name: String,
    label: String,
    ftype: FieldType,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    help: Option<String>,
}

#[derive(Deserialize)]
struct ArgsPayload {
    entity: String,
    schema: Vec<ArgsFieldDef>,
    records: Vec<serde_json::Map<String, Value>>,
    #[serde(default)]
    projections: Vec<ProjectionSpec>,
    #[serde(default)]
    active: Option<ProjectionSpec>,
}

/// Parse `present_view` tool-call args into a fresh `LlmProjected` [`DataView`].
///
/// # Errors
/// Returns an error when the args are unparseable (even after the repair pass) or fail the
/// minimal non-empty-`entity`/non-empty-`schema` invariant — never panics.
pub fn parse_present_view_args(args: &Value) -> Result<DataView> {
    match try_parse(args) {
        Ok(view) => Ok(view),
        Err(first_err) => {
            let repaired = repair_value(args);
            try_parse(&repaired)
                .with_context(|| format!("present_view args invalid (first attempt: {first_err})"))
        }
    }
}

fn try_parse(args: &Value) -> Result<DataView> {
    let payload: ArgsPayload =
        serde_json::from_value(args.clone()).context("parsing present_view args")?;
    build_data_view(payload)
}

fn build_data_view(payload: ArgsPayload) -> Result<DataView> {
    if payload.entity.trim().is_empty() {
        bail!("present_view: `entity` must not be empty");
    }
    if payload.schema.is_empty() {
        bail!("present_view: `schema` must include at least one field");
    }

    let schema: Vec<FieldDef> = payload
        .schema
        .into_iter()
        .map(|f| FieldDef {
            name: f.name,
            label: f.label,
            ftype: f.ftype,
            writable: false,
            required: f.required,
            help: f.help,
        })
        .collect();

    // Normalize: default to a single Table layout when the model omitted `projections`, and
    // to the first available layout when it omitted `active` — a view with no renderable
    // layout at all is never emitted (see the phase's unknown-kind→Table wire contract).
    // Dedup by `kind`, keeping the first occurrence: the GUI's projection switcher keys its
    // list by `kind` (Svelte `{#each ... (spec.kind)}`), so a model repeating a kind — nothing
    // stops it, this field is entirely model-authored — would otherwise hand the renderer a
    // duplicate key and break the pane.
    let mut projections: Vec<ProjectionSpec> = Vec::new();
    for p in payload.projections {
        if !projections.iter().any(|existing: &ProjectionSpec| existing.kind == p.kind) {
            projections.push(p);
        }
    }
    if projections.is_empty() {
        projections.push(ProjectionSpec {
            kind: ProjectionKind::Table,
            binding: None,
        });
    }
    let active = payload.active.unwrap_or_else(|| projections[0].clone());

    Ok(DataView {
        view_id: Uuid::new_v4(),
        entity: payload.entity,
        schema,
        records: payload.records,
        projections,
        active,
        total: None,
        cursor: None,
        provenance: ViewProvenance::LlmProjected,
    })
}

/// `FieldType` variant names with no payload — safe to infer from a bare string the model
/// wrote instead of the tagged `{"type":"Text"}` wire shape. `Money`/`Enum`/`Reference` carry
/// required payload data no bare string could supply, so they are deliberately excluded: a
/// model that drops their payload fails cleanly on the second parse rather than getting a
/// fabricated one.
const BARE_FTYPE_NAMES: &[&str] = &[
    "Text", "LongText", "Int", "Float", "Bool", "Date", "DateTime", "Tags", "Email", "Phone",
    "Url", "Opaque",
];

const PROJECTION_KIND_NAMES: &[&str] = &["Table", "Cards", "Kanban", "Calendar", "Chart"];

/// Repairs the two weak-model deviations `present_view` actually sees in practice: (1) the
/// whole payload double-encoded as a JSON string instead of an object, and (2) `ftype`/`kind`
/// given as a bare (possibly wrong-case) variant name instead of the wire's tagged shape. Any
/// shape not recognized here is passed through unchanged, so the retried parse fails cleanly
/// rather than the repair inventing data — this is a bounded, best-effort pass, not a general
/// coercion layer.
fn repair_value(args: &Value) -> Value {
    let mut v = args.clone();
    if let Some(s) = v.as_str() {
        if let Ok(parsed) = serde_json::from_str::<Value>(s.trim()) {
            v = parsed;
        }
    }
    let Some(obj) = v.as_object_mut() else {
        return v;
    };
    if let Some(Value::Array(fields)) = obj.get_mut("schema") {
        for f in fields.iter_mut().filter_map(Value::as_object_mut) {
            normalize_bare_variant(f, "ftype", BARE_FTYPE_NAMES);
        }
    }
    if let Some(Value::Array(projs)) = obj.get_mut("projections") {
        for p in projs.iter_mut().filter_map(Value::as_object_mut) {
            normalize_kind_string(p);
        }
    }
    if let Some(active) = obj.get_mut("active").and_then(Value::as_object_mut) {
        normalize_kind_string(active);
    }
    v
}

/// Rewrites `field[key]` from a bare (case-insensitive) variant-name string into the tagged
/// `{"type": "<ExactName>"}` shape `FieldType`'s `#[serde(tag = "type")]` deserializer expects.
fn normalize_bare_variant(
    field: &mut serde_json::Map<String, Value>,
    key: &str,
    known: &[&str],
) {
    if let Some(Value::String(s)) = field.get(key) {
        if let Some(exact) = known.iter().find(|k| k.eq_ignore_ascii_case(s)) {
            field.insert(key.to_string(), serde_json::json!({ "type": exact }));
        }
    }
}

/// `ProjectionKind` has no payload variants, so `kind` only ever needs case-normalizing (never
/// the bare-string → tagged-object rewrite `normalize_bare_variant` does for `ftype`).
fn normalize_kind_string(obj: &mut serde_json::Map<String, Value>) {
    if let Some(Value::String(s)) = obj.get("kind") {
        if let Some(exact) = PROJECTION_KIND_NAMES.iter().find(|k| k.eq_ignore_ascii_case(s)) {
            obj.insert("kind".to_string(), Value::String((*exact).to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_args() -> Value {
        json!({
            "entity": "contact",
            "schema": [
                { "name": "name", "label": "Name", "ftype": { "type": "Text" } },
                { "name": "balance", "label": "Balance", "ftype": { "type": "Money", "data": { "currency": "USD" } } }
            ],
            "records": [
                { "name": "Acme Corp", "balance": 100 }
            ]
        })
    }

    #[test]
    fn parses_clean_args_and_defaults_projections_and_active() {
        let view = parse_present_view_args(&valid_args()).expect("valid args must parse");
        assert_eq!(view.entity, "contact");
        assert_eq!(view.schema.len(), 2);
        assert!(!view.schema[0].writable, "writable must always be forced false");
        assert_eq!(view.records.len(), 1);
        assert_eq!(view.projections.len(), 1);
        assert_eq!(view.projections[0].kind, ProjectionKind::Table);
        assert_eq!(view.active.kind, ProjectionKind::Table);
        assert_eq!(view.provenance, ViewProvenance::LlmProjected);
    }

    #[test]
    fn duplicate_projection_kinds_are_deduped_keeping_the_first() {
        let mut args = valid_args();
        args["projections"] = json!([
            { "kind": "Table" },
            { "kind": "Cards", "binding": "name" },
            { "kind": "Table" },
        ]);
        let view = parse_present_view_args(&args).expect("valid args must parse");
        assert_eq!(
            view.projections.len(),
            2,
            "repeated kinds must collapse to one entry each — the GUI keys its switcher by kind"
        );
        assert_eq!(view.projections[0].kind, ProjectionKind::Table);
        assert_eq!(view.projections[1].kind, ProjectionKind::Cards);
    }

    #[test]
    fn repairs_bare_ftype_string_and_lowercase_kind() {
        let mut args = valid_args();
        args["schema"][0]["ftype"] = json!("text"); // bare, wrong-case string
        args["projections"] = json!([{ "kind": "table" }]); // lowercase
        args["active"] = json!({ "kind": "table" });
        let view = parse_present_view_args(&args).expect("repair pass must recover this shape");
        assert_eq!(view.schema[0].ftype, FieldType::Text);
        assert_eq!(view.active.kind, ProjectionKind::Table);
    }

    #[test]
    fn repairs_double_encoded_string_payload() {
        let stringified = Value::String(valid_args().to_string());
        let view = parse_present_view_args(&stringified).expect("stringified payload must repair");
        assert_eq!(view.entity, "contact");
    }

    #[test]
    fn malformed_args_return_a_clean_err_not_a_panic() {
        let bad = json!({ "not_entity": true, "definitely_not_schema": 42 });
        let result = parse_present_view_args(&bad);
        assert!(result.is_err(), "malformed args with no repairable shape must return Err");
    }

    #[test]
    fn empty_entity_is_rejected() {
        let mut args = valid_args();
        args["entity"] = json!("   ");
        assert!(parse_present_view_args(&args).is_err());
    }

    #[test]
    fn empty_schema_is_rejected() {
        let mut args = valid_args();
        args["schema"] = json!([]);
        assert!(parse_present_view_args(&args).is_err());
    }
}
