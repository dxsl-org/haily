//! Frozen `OdooExecutor` wire shapes (Phase 3, M5b) — captured NOW, while `OdooExecutor`
//! still exists, so a generic-interpreter parity test survives Phase 4a's deletion of it.
//! Every value here is a HAND-VERIFIED snapshot of what `OdooExecutor::rpc` / `create_args` /
//! `read_back` actually produce (`crates/haily-tools/src/connector/odoo_executor.rs`), not a
//! re-derivation from code that could silently drift alongside it. NOT itself a `tests/*.rs`
//! file (cargo does not compile `tests/fixtures/*` as its own test binary) — included via
//! `#[path = "fixtures/odoo_wire_fixtures.rs"] mod odoo_wire_fixtures;` from an actual
//! integration test file.
use serde_json::{json, Value};

pub const DB: &str = "haily_ci";
pub const UID: i64 = 2;
pub const KEY: &str = "SECRET-TEST-KEY";
pub const LANG: &str = "en_US";
pub const TZ: &str = "UTC";

/// The `execute_kw` JSON-RPC envelope `OdooExecutor::rpc` builds for ANY (model, method,
/// args, kwargs) — the wrapper every op shares.
#[must_use]
pub fn execute_kw_envelope(db: &str, uid: i64, key: &str, model: &str, method: &str, args: Value, kwargs: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "call",
        "id": null,
        "params": {
            "service": "object",
            "method": "execute_kw",
            "args": [db, uid, key, model, method, args, kwargs],
        }
    })
}

/// The kwargs `OdooExecutor::base_kwargs` sends on every `call` (locale context only — no
/// `limit`/`active_test`, those are read-back-only per `OdooExecutor::read_back`).
#[must_use]
pub fn call_kwargs() -> Value {
    json!({ "context": { "lang": LANG, "tz": TZ } })
}

/// `odoo_contact_create`'s expected `args`: `create_args` embeds the correlation ref into the
/// model's `ref` field, then wraps as `[vals]`.
#[must_use]
pub fn contact_create_args(name: &str, correlation_ref: &str) -> Value {
    json!([{ "name": name, "ref": correlation_ref }])
}

/// `odoo_contact_update`'s expected `args`: `write([ids], values)`.
#[must_use]
pub fn contact_update_args(id: i64, values: Value) -> Value {
    json!([[id], values])
}

/// A compensation `unlink`'s expected `args`: `unlink([ids])`.
#[must_use]
pub fn unlink_args(id: i64) -> Value {
    json!([[id]])
}

/// Frozen fault-classification pairs straight off `odoo_fault::classify`'s recognized set:
/// `(data.name, expected normalized token)`. The last entry is deliberately UNrecognized —
/// fail-closed to `UnknownError`.
pub const FAULT_TOKENS: &[(&str, &str)] = &[
    ("odoo.exceptions.AccessError", "AccessError"),
    ("odoo.exceptions.ValidationError", "ValidationError"),
    ("odoo.exceptions.MissingError", "MissingError"),
    ("odoo.exceptions.SomeNewException", "UnknownError"),
];

/// The `search_read` domain `OdooExecutor::read_back` builds when an `id_hint` is known.
#[must_use]
pub fn readback_domain_by_id(id: i64) -> Value {
    json!([[["id", "=", id]]])
}

/// The domain built from the model's correlation field when no `id_hint` is known.
#[must_use]
pub fn readback_domain_by_correlation(field: &str, correlation_ref: &str) -> Value {
    json!([[[field, "=", correlation_ref]]])
}

/// The domain built with neither locator — record not locatable.
#[must_use]
pub fn readback_domain_empty() -> Value {
    json!([])
}
