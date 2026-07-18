//! `parameters_schema()` for `present_view` — the projectable subset of [`haily_types::DataView`]
//! the model is allowed to author. Deliberately mirrors `DataView`'s real wire encoding (the
//! `{"type":...,"data":...}` tag/content shape `FieldType` serializes as, and the bare-string
//! unit-variant shape `ProjectionKind` serializes as) so a well-formed call deserializes straight
//! into the wire types with no translation layer — `parse.rs` only has to repair deviations from
//! this shape, never reinterpret a parallel one.
//!
//! Only the ~8 load-bearing `FieldType` variants are listed in the `type` enum below (View
//! Engine Phase A's renderer need, per plan) — the full 14-variant vocabulary exists on the wire
//! (frozen in Phase 1) but gold-plating the schema/grammar for `Tags`/`Email`/`Phone`/`Url`/
//! `Float`/`DateTime` buys nothing this phase actually renders.

use serde_json::{json, Value};

/// The `FieldType` variant names this tool's schema exposes to the model. Kept in sync with
/// `parse::BARE_FTYPE_NAMES`'s case-insensitive repair list.
pub const LOAD_BEARING_FTYPES: &[&str] = &[
    "Text", "LongText", "Int", "Money", "Bool", "Date", "Enum", "Reference", "Opaque",
];

/// The full `ProjectionKind` vocabulary (all five — Phase A renders only Table/Cards, but the
/// wire type is closed to exactly these five, so the schema lists all of them for correct
/// round-tripping of a value the model happens to pick outside the rendered subset).
const PROJECTION_KINDS: &[&str] = &["Table", "Cards", "Kanban", "Calendar", "Chart"];

fn ftype_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            // Named "type"/"data" to match FieldType's `#[serde(tag = "type", content = "data")]`
            // wire encoding exactly — a nested property literally named "type" is ordinary JSON
            // Schema, not a collision with this schema's own "type" keyword.
            "type": { "enum": LOAD_BEARING_FTYPES },
            // Payload for Money{currency}/Enum{variants}/Reference{entity}; absent/generic for
            // the six payload-less variants. Left untyped (no "type"/"enum") so the GBNF
            // generator treats it as an unconstrained JSON value rather than skipping the tool.
            "data": {}
        },
        "required": ["type"]
    })
}

fn projection_spec_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": { "enum": PROJECTION_KINDS },
            "binding": { "type": "string" }
        },
        "required": ["kind"]
    })
}

fn field_def_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "description": "Wire/column key of this field." },
            "label": { "type": "string", "description": "Human-facing column/attribute label." },
            "ftype": ftype_schema(),
            "required": { "type": "boolean" },
            "help": { "type": "string" }
        },
        "required": ["name", "label", "ftype"]
    })
}

/// `present_view`'s `parameters_schema()` — see module docs for the design rationale.
pub fn present_view_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "entity": {
                "type": "string",
                "description": "Human label for the dataset being projected, e.g. \"contact\" or \"task\"."
            },
            "schema": {
                "type": "array",
                "items": field_def_schema(),
                "description": "Column/attribute definitions describing every key present in `records`."
            },
            "records": {
                "type": "array",
                "items": {},
                "description": "The rows to display — each a JSON object whose keys match `schema[].name`."
            },
            "projections": {
                "type": "array",
                "items": projection_spec_schema(),
                "description": "Layouts this view can render as. Defaults to a single Table layout if omitted."
            },
            "active": {
                "type": "object",
                "properties": {
                    "kind": { "enum": PROJECTION_KINDS },
                    "binding": { "type": "string" }
                },
                "required": ["kind"],
                "description": "Which layout is initially shown. Defaults to the first of `projections` if omitted."
            }
        },
        "required": ["entity", "schema", "records"]
    })
}

// GBNF-supported-subset coverage for this exact schema is asserted end-to-end by
// `haily-core/tests/gbnf_tool_schemas.rs` (`every_v1_and_coding_tool_schema_produces_a_grammar`),
// which sweeps every tool in `ToolRegistry::build_v1()` — haily-tools has no dependency on
// haily-llm to duplicate that check here (would require a new cross-crate dependency for no
// additional coverage).
