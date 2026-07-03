//! Odoo golden tests (Safe Operator Harness phase 5) — the end-to-end R3 proof against a
//! LIVE Odoo sandbox (odoo:18.0 + postgres:15, see docker/odoo-ci-compose.yml).
//!
//! EVERY test is gated on `HAILY_ODOO_URL`: with it UNSET the test early-returns after a
//! `println!` SKIP marker, so `cargo test --workspace` stays green on a machine with no
//! Docker (the offline fault-classifier unit tests live in the lib crate and always run).
//! CI wires this up via scripts/odoo-ci-bootstrap.sh (authenticated-RPC readiness → scoped
//! user → generated key → exported env), in a NON-BLOCKING / nightly job.
//!
//! The tests insert a connector manifest whose `allowed_ip_cidrs` pins the sandbox host IP
//! (`127.0.0.1/32`) so the phase-4 SSRF allowance permits localhost:8069 IN CI ONLY — via
//! the TEST-ONLY `allow_loopback` flag on the executor (never true in production; a real
//! Odoo host is public and hits the normal guard, never this pin).
//!
//! The write/undo scenarios drive the REAL production path: create/update/archive go through
//! `HttpConnectorTool::execute` (which captures pre_state, journals the REAL compensation
//! plan BEFORE the call, and writes a create's returned id back into that plan), then
//! `journal_undo` is driven against THAT journal row. No hand-built compensation plans — the
//! test proves the same code a live agent runs, so a broken create→archive undo fails here.
//!
//! Coverage: create→read; create→undo (created record archived); update→undo (previous vals
//! restored, own read-back, write_date matched — C10); archive→undo (active flipped back);
//! lost-response reconciliation by correlation_ref (C7); unlink-compensation MissingError =
//! already-done (M4); batch partial failure three counts; no-secret-in-journal (C4).
use haily_db::queries::{journal, meta};
use haily_db::DbHandle;
use haily_tools::connector::odoo_executor::{OdooExecutor, OdooExecutorConfig};
use haily_tools::connector::{
    manifest, ConnectorExecutor, ExecOutcome, HttpConnectorTool, Manifest,
};
use haily_tools::journal_undo::{attempt_undo, batch_undo, UndoOutcome};
use haily_tools::{Tool, ToolContext};
use haily_types::ApprovalGate;
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// SKIP guard: `Some(url)` when the sandbox is configured, else `None` and the caller
/// early-returns. Keeping the whole matrix behind one env var means a Docker-less
/// `cargo test --workspace` never fails on these.
fn odoo_url() -> Option<String> {
    std::env::var("HAILY_ODOO_URL").ok().filter(|u| !u.is_empty())
}

fn odoo_db() -> String {
    std::env::var("HAILY_ODOO_DB").unwrap_or_else(|_| "haily_ci".to_string())
}

fn odoo_uid() -> i64 {
    std::env::var("HAILY_ODOO_UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2)
}

/// The cred-ref preference key (C4): the journal records THIS name, never the key value.
const CRED_REF: &str = "connector.odoo.api_key";

/// Build the phase-5 Odoo manifest pinned to the sandbox host IP so the SSRF allowance
/// permits localhost in CI. The `base_url` comes from the env so the golden tests target the
/// live sandbox, not the placeholder in connectors/odoo-crm.manifest.json.
fn ci_manifest(base_url: &str) -> Manifest {
    // Reuse the SHIPPED manifest's ops (the 11 CRM ops) but override base_url + allowance for
    // the sandbox — the ops/tiers/compensations are exactly what production would register.
    let shipped = include_str!("../../../connectors/odoo-crm.manifest.json");
    let mut m: Value = serde_json::from_str(shipped).expect("shipped manifest parses");
    m["base_url"] = json!(base_url);
    m["allowed_ip_cidrs"] = json!(["127.0.0.1/32", "::1/128"]);
    manifest::parse(&m.to_string()).expect("ci manifest parses")
}

/// Init a fresh DB + seed the Odoo API key preference (the executor reads it by reference).
async fn setup() -> (Arc<DbHandle>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
    let key = std::env::var("HAILY_ODOO_API_KEY").unwrap_or_default();
    meta::upsert_preference(&db, CRED_REF, &key, "ci").await.unwrap();
    (Arc::new(db), dir)
}

/// Build the live Odoo executor with the TEST-ONLY loopback allowance ENABLED so it can reach
/// the sandbox at 127.0.0.1:8069. This flag is never set in production — the production wiring
/// (`Orchestrator::init`) constructs its executor with `allow_loopback=false`.
fn odoo_executor(db: Arc<DbHandle>, manifest: Arc<Manifest>) -> Arc<OdooExecutor> {
    Arc::new(OdooExecutor::new(OdooExecutorConfig {
        manifest,
        cred_ref: CRED_REF.to_string(),
        db,
        db_name: odoo_db(),
        uid: odoo_uid(),
        lang: "en_US".to_string(),
        tz: "UTC".to_string(),
        kill: Arc::new(AtomicBool::new(false)),
        timeout: Duration::from_secs(15),
        allow_loopback: true, // TEST ONLY — reach the local sandbox; never true in production.
    }))
}

/// A throwaway approval gate (auto-denies). The golden connector tools are executed directly,
/// so no approval is ever raised; the gate only exists because `ToolContext` requires one.
struct NoopGate;
#[async_trait::async_trait]
impl ApprovalGate for NoopGate {
    async fn request(
        &self,
        _approval_id: uuid::Uuid,
        _session_id: uuid::Uuid,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> bool {
        false
    }
}

/// Build a `ToolContext` for driving `HttpConnectorTool::execute`. The connector tool never
/// touches kms, but the context requires a handle, so a throwaway one is initialized on the
/// same tempdir (kept alive by the returned guard).
async fn tool_ctx(db: Arc<DbHandle>) -> (ToolContext, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let kms_db = DbHandle::init(&dir.path().join("kms.db")).await.unwrap();
    let kms = Arc::new(haily_kms::KmsHandle::init(kms_db, dir.path()).await.unwrap());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let ctx = ToolContext {
        db,
        kms,
        session_id: uuid::Uuid::new_v4(),
        depth: 0,
        domain: None,
        approval_gate: Arc::new(NoopGate),
        approval_tx: tx,
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    (ctx, dir)
}

/// Build the `HttpConnectorTool` for one manifest op, driven by the live Odoo executor. This
/// is exactly what `register_connectors` wires in production (minus the loopback flag).
fn connector_tool(
    manifest: &Arc<Manifest>,
    op_name: &str,
    exec: Arc<dyn ConnectorExecutor>,
) -> HttpConnectorTool {
    let op = manifest
        .ops
        .iter()
        .find(|o| o.name == op_name)
        .unwrap_or_else(|| panic!("manifest op {op_name} not found"));
    HttpConnectorTool {
        manifest: Arc::clone(manifest),
        op: Arc::new(op.clone()),
        executor: exec,
        kill: Arc::new(AtomicBool::new(false)),
        cred_ref: CRED_REF.to_string(),
    }
}

/// Run one connector op through the REAL tool path and return the journal row it recorded
/// (the outbox row with the REAL compensation plan). The most recent row for the session is
/// the one just written.
async fn run_op_and_row(
    tool: &HttpConnectorTool,
    ctx: &ToolContext,
    params: Value,
) -> journal::ActionJournalRow {
    tool.execute(json!({ "params": params }), ctx)
        .await
        .expect("connector op executes");
    let rows = journal::list_by_session(&ctx.db, &ctx.session_id.to_string())
        .await
        .unwrap();
    rows.into_iter().next().expect("journal row recorded")
}

/// Resolve the install-specific `ir.model` id for `model` (e.g. `res.partner`) at test time.
/// `mail.activity.res_model_id` is a mandatory many2one → `ir.model`; the id is NOT stable
/// across installs (module install order), so it must be looked up, never hardcoded.
///
/// Driven through the executor's own `call` path: an op keyword that is not a manifest op
/// name resolves its model+method from the params, and the non-CRUD `search_read` method
/// falls through to the verbatim-`args` branch — no separate raw-RPC helper needed.
async fn ir_model_id(exec: &dyn ConnectorExecutor, model: &str) -> i64 {
    let params = json!({
        "model": "ir.model",
        "method": "search_read",
        "args": [[["model", "=", model]], ["id"]],
    });
    match exec.call("read", &params).await.expect("ir.model search_read") {
        ExecOutcome::Ok { returned_id, .. } => returned_id
            .expect("ir.model row exists")
            .parse::<i64>()
            .expect("ir.model id is numeric"),
        other => panic!("ir.model lookup faulted: {other:?}"),
    }
}

/// Create a `res.partner` through the real tool path and return its id — a valid `res_id`
/// anchor for the activity (odoo_uid() is a res.users uid, not necessarily a partner id).
async fn create_partner(exec: &dyn ConnectorExecutor, name: &str) -> i64 {
    let corr = uuid::Uuid::new_v4().to_string();
    let created = exec
        .call(
            "odoo_contact_create",
            &json!({"correlation_ref": corr, "values": {"name": name}}),
        )
        .await
        .expect("create partner anchor");
    match created {
        ExecOutcome::Ok { returned_id, .. } => returned_id
            .expect("partner create returns id")
            .parse::<i64>()
            .expect("partner id is numeric"),
        other => panic!("partner create faulted: {other:?}"),
    }
}

// ------------------------------------------------------------------------------------------
// Offline manifest-parse test — ALWAYS runs (no Odoo). Asserts the shipped manifest loads via
// the phase-4 parser with exactly 11 ops and the right tiers.
// ------------------------------------------------------------------------------------------

#[test]
fn shipped_manifest_parses_with_11_ops_and_correct_tiers() {
    let shipped = include_str!("../../../connectors/odoo-crm.manifest.json");
    let m = manifest::parse(shipped).expect("odoo-crm.manifest.json must parse via phase-4 parser");
    assert_eq!(m.connector_name, "odoo-crm");
    assert_eq!(m.ops.len(), 11, "11 v1 ops");
    // `unlink` is EXCLUDED — no op should route to a bare unlink method.
    assert!(
        !m.ops.iter().any(|o| o.name.ends_with("_unlink")),
        "unlink op must be excluded (no safe compensation)"
    );

    let tier = |name: &str| {
        m.ops
            .iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| panic!("missing op {name}"))
            .risk_tier()
    };
    use haily_tools::RiskTier;
    // Reads → Read.
    assert_eq!(tier("odoo_contact_read"), RiskTier::Read);
    assert_eq!(tier("odoo_lead_read"), RiskTier::Read);
    assert_eq!(tier("odoo_activity_read"), RiskTier::Read);
    // Reversible writes → ReversibleWrite.
    assert_eq!(tier("odoo_contact_create"), RiskTier::ReversibleWrite);
    assert_eq!(tier("odoo_contact_update"), RiskTier::ReversibleWrite);
    assert_eq!(tier("odoo_contact_archive"), RiskTier::ReversibleWrite);
    assert_eq!(tier("odoo_lead_create"), RiskTier::ReversibleWrite);
    assert_eq!(tier("odoo_activity_create"), RiskTier::ReversibleWrite);
    // activity.done is final/irreversible.
    assert_eq!(tier("odoo_activity_done"), RiskTier::IrreversibleWrite);
    assert_eq!(
        m.ops.iter().find(|o| o.name == "odoo_activity_done").unwrap().compensability_str(),
        "final",
        "activity.done has no compensation"
    );
    // create compensations archive (op=archive); update compensations write-back (op=write).
    let contact_create = m.ops.iter().find(|o| o.name == "odoo_contact_create").unwrap();
    assert_eq!(
        contact_create.compensation.as_ref().unwrap()["op"],
        json!("archive")
    );
}

// ------------------------------------------------------------------------------------------
// Golden matrix — gated on HAILY_ODOO_URL. Each early-returns (SKIP) when unset.
// ------------------------------------------------------------------------------------------

macro_rules! require_odoo {
    // Binds the sandbox URL for tests that build an executor against it.
    ($url:ident) => {
        let Some($url) = odoo_url() else {
            println!("SKIP: HAILY_ODOO_URL unset — Odoo golden test skipped (no sandbox).");
            return;
        };
    };
    // Bindless form: gate the test on the sandbox being present without needing the URL.
    () => {
        if odoo_url().is_none() {
            println!("SKIP: HAILY_ODOO_URL unset — Odoo golden test skipped (no sandbox).");
            return;
        }
    };
}

#[tokio::test]
async fn create_read_roundtrip() {
    require_odoo!(url);
    let (db, _d) = setup().await;
    let m = Arc::new(ci_manifest(&url));
    let exec = odoo_executor(Arc::clone(&db), Arc::clone(&m));

    let corr = uuid::Uuid::new_v4().to_string();
    let params = json!({"correlation_ref": corr, "values": {"name": "Golden Alice"}});
    let outcome = exec.call("odoo_contact_create", &params).await.expect("create");
    let id = match outcome {
        ExecOutcome::Ok { returned_id, .. } => returned_id.expect("create returns id"),
        other => panic!("create faulted: {other:?}"),
    };
    assert!(!id.is_empty());

    // Read-back by correlation_ref finds the record we just wrote.
    let back = exec.read_back("odoo_contact_read", &corr, None, None).await.expect("read-back");
    assert_eq!(back.get("name").and_then(Value::as_str), Some("Golden Alice"));
    assert!(back.get("write_date").is_some(), "write_date present for C10");
}

#[tokio::test]
async fn create_undo_roundtrip() {
    require_odoo!(url);
    let (db, _d) = setup().await;
    let m = Arc::new(ci_manifest(&url));
    let exec = odoo_executor(Arc::clone(&db), Arc::clone(&m)) as Arc<dyn ConnectorExecutor>;
    let (ctx, _kd) = tool_ctx(Arc::clone(&db)).await;

    // Create THROUGH the real tool: it journals the archive compensation plan BEFORE the call
    // with NO id, then writes the RETURNED id back into that plan (FIX 1). If that write-back
    // is broken the undo below archives nothing / the wrong record and the assertion fails.
    let create_tool = connector_tool(&m, "odoo_contact_create", Arc::clone(&exec));
    let corr = uuid::Uuid::new_v4().to_string();
    let row = run_op_and_row(
        &create_tool,
        &ctx,
        json!({"correlation_ref": corr, "values": {"name": "Created Zoe"}}),
    )
    .await;
    // The recorded plan must carry the concrete created id — proof the write-back landed.
    let plan: Value =
        serde_json::from_str(row.compensation_plan.as_deref().unwrap()).unwrap();
    assert!(
        plan.get("id").is_some() || plan.get("ids").is_some(),
        "create's compensation plan must carry the returned id after write-back: {plan}"
    );

    // Confirm the record is live before undo.
    let before = exec.read_back("odoo_contact_read", &corr, None, None).await.unwrap();
    assert_eq!(before.get("active").and_then(Value::as_bool), Some(true));

    // Drive the REAL undo against the REAL row — the archive compensation must target the
    // created id and flip active=false.
    let outcome = attempt_undo(&db, exec.as_ref(), &row, &row.session_id)
        .await
        .expect("undo");
    assert!(
        matches!(outcome, UndoOutcome::Undone | UndoOutcome::AlreadyDone),
        "create-undo: {outcome:?}"
    );
    // Read back the archived record explicitly (active_test disabled) to prove it was archived,
    // not deleted / untouched.
    let after = exec
        .read_back("odoo_contact_read", &corr, Some("res.partner"), None)
        .await
        .unwrap();
    assert_eq!(
        after.get("active").and_then(Value::as_bool),
        Some(false),
        "the created record must be archived by the undo: {after}"
    );
}

#[tokio::test]
async fn update_undo_roundtrip() {
    require_odoo!(url);
    let (db, _d) = setup().await;
    let m = Arc::new(ci_manifest(&url));
    let exec = odoo_executor(Arc::clone(&db), Arc::clone(&m)) as Arc<dyn ConnectorExecutor>;
    let (ctx, _kd) = tool_ctx(Arc::clone(&db)).await;

    // Create a contact with a known `function`, capture its id.
    let corr = uuid::Uuid::new_v4().to_string();
    let created = exec
        .call(
            "odoo_contact_create",
            &json!({"correlation_ref": corr, "values": {"name": "Undo Bob", "function": "before"}}),
        )
        .await
        .expect("create");
    let id = match created {
        ExecOutcome::Ok { returned_id, .. } => returned_id.unwrap().parse::<i64>().unwrap(),
        other => panic!("{other:?}"),
    };

    // Update THROUGH the real tool: it reads pre_state (function="before"), journals a
    // write-back compensation whose `values` are lifted from pre_state (FIX 2 real path), then
    // writes function="after".
    let update_tool = connector_tool(&m, "odoo_contact_update", Arc::clone(&exec));
    let row = run_op_and_row(
        &update_tool,
        &ctx,
        json!({"ids": [id], "values": {"function": "after"}}),
    )
    .await;
    // The recorded plan must restore the PREVIOUS value from pre_state, not be an empty write.
    let plan: Value =
        serde_json::from_str(row.compensation_plan.as_deref().unwrap()).unwrap();
    assert_eq!(
        plan.pointer("/values/function").and_then(Value::as_str),
        Some("before"),
        "update compensation must restore the previous value from pre_state: {plan}"
    );

    // Drive the REAL undo — function must be restored to "before", C10-guarded by write_date.
    let outcome = attempt_undo(&db, exec.as_ref(), &row, &row.session_id)
        .await
        .expect("undo");
    assert!(
        matches!(outcome, UndoOutcome::Undone | UndoOutcome::AlreadyDone),
        "update-undo: {outcome:?}"
    );
    let after = exec.read_back("odoo_contact_read", &corr, None, None).await.unwrap();
    assert_eq!(after.get("name").and_then(Value::as_str), Some("Undo Bob"));
    assert_eq!(
        after.get("function").and_then(Value::as_str),
        Some("before"),
        "previous value must be restored by the real undo path: {after}"
    );
}

#[tokio::test]
async fn archive_undo_roundtrip() {
    require_odoo!(url);
    let (db, _d) = setup().await;
    let m = Arc::new(ci_manifest(&url));
    let exec = odoo_executor(Arc::clone(&db), Arc::clone(&m)) as Arc<dyn ConnectorExecutor>;
    let (ctx, _kd) = tool_ctx(Arc::clone(&db)).await;

    let corr = uuid::Uuid::new_v4().to_string();
    let created = exec
        .call(
            "odoo_contact_create",
            &json!({"correlation_ref": corr, "values": {"name": "Archive Carol"}}),
        )
        .await
        .expect("create");
    let id = match created {
        ExecOutcome::Ok { returned_id, .. } => returned_id.unwrap().parse::<i64>().unwrap(),
        other => panic!("{other:?}"),
    };

    // Archive THROUGH the real tool (active:false). Its compensation flips active back to true
    // and targets the request's ids.
    let archive_tool = connector_tool(&m, "odoo_contact_archive", Arc::clone(&exec));
    let row = run_op_and_row(
        &archive_tool,
        &ctx,
        json!({"ids": [id], "values": {"active": false}}),
    )
    .await;

    // Undo → active flips back to true.
    let outcome = attempt_undo(&db, exec.as_ref(), &row, &row.session_id)
        .await
        .expect("undo");
    assert!(
        matches!(outcome, UndoOutcome::Undone | UndoOutcome::AlreadyDone),
        "archive-undo: {outcome:?}"
    );
    let after = exec.read_back("odoo_contact_read", &corr, None, None).await.unwrap();
    assert_eq!(
        after.get("active").and_then(Value::as_bool),
        Some(true),
        "archive undo must flip active back to true: {after}"
    );
}

#[tokio::test]
async fn lost_response_reconciles_via_correlation_ref() {
    require_odoo!(url);
    let (db, _d) = setup().await;
    let m = Arc::new(ci_manifest(&url));
    let exec = odoo_executor(Arc::clone(&db), Arc::clone(&m));

    // Simulate a "lost response": the create landed but we act as if we never saw the id.
    // C7 recovery is read_back by correlation_ref — it must find the record.
    let corr = uuid::Uuid::new_v4().to_string();
    exec.call("odoo_contact_create", &json!({"correlation_ref": corr, "values": {"name": "Lost Dan"}}))
        .await
        .expect("create");
    // Read-back by correlation_ref reconciles — the write is DISCOVERABLE, not "failed" (C7).
    let back = exec.read_back("odoo_contact_read", &corr, None, None).await.expect("read-back reconciles");
    assert_eq!(back.get("name").and_then(Value::as_str), Some("Lost Dan"));
    assert!(back.get("id").is_some(), "reconciled by correlation_ref → id known");
}

#[tokio::test]
async fn unlink_compensation_missing_error_is_done() {
    require_odoo!(url);
    let (db, _d) = setup().await;
    let m = Arc::new(ci_manifest(&url));
    let exec = odoo_executor(Arc::clone(&db), Arc::clone(&m)) as Arc<dyn ConnectorExecutor>;
    let (ctx, _kd) = tool_ctx(Arc::clone(&db)).await;

    // Create an activity THROUGH the real tool (journals an unlink compensation carrying the
    // returned id + model=mail.activity), then delete it out-of-band so the unlink faults with
    // MissingError — which M4 treats as ALREADY-DONE (success), not a retryable failure.
    //
    // `mail.activity` requires the many2one `res_model_id` (FK → ir.model), NOT the `res_model`
    // STRING — setting the string does not populate the mandatory FK. The ir.model id is
    // install-specific (depends on module install order), so resolve it at test time instead of
    // hardcoding. `res_id` must be a valid `res.partner` id: create one through the tool and use
    // its returned id (odoo_uid() is a res.users uid, not necessarily a valid partner id).
    let res_model_id = ir_model_id(exec.as_ref(), "res.partner").await;
    let partner_id = create_partner(exec.as_ref(), "Ghost Anchor").await;

    let create_tool = connector_tool(&m, "odoo_activity_create", Arc::clone(&exec));
    let corr = uuid::Uuid::new_v4().to_string();
    let row = run_op_and_row(
        &create_tool,
        &ctx,
        json!({"correlation_ref": corr, "values": {"summary": "Ghost", "res_model_id": res_model_id, "res_id": partner_id}}),
    )
    .await;
    // The compensation model MUST be mail.activity — this is the FIX 4 model-routing proof:
    // the unlink read-back must query mail.activity, not the manifest's first model res.partner.
    let plan: Value =
        serde_json::from_str(row.compensation_plan.as_deref().unwrap()).unwrap();
    assert_eq!(
        plan.get("model").and_then(Value::as_str),
        Some("mail.activity"),
        "unlink compensation must carry model=mail.activity for correct read-back routing: {plan}"
    );
    // Extract the created id to delete it out-of-band.
    let id = plan
        .get("id")
        .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_i64().map(|n| n.to_string())))
        .or_else(|| plan.pointer("/ids/0").map(|v| v.to_string()));

    // Delete it out-of-band (mark the activity done → gone) so the compensation hits a gone
    // record and the server faults with MissingError.
    if let Some(id) = &id {
        let _ = exec.call("odoo_activity_done", &json!({"ids": [id.parse::<i64>().unwrap_or(0)]})).await;
    }

    // The unlink of an already-gone id must classify as AlreadyDone (MissingError = done).
    let outcome = attempt_undo(&db, exec.as_ref(), &row, &row.session_id)
        .await
        .expect("undo");
    assert!(
        matches!(outcome, UndoOutcome::AlreadyDone),
        "MissingError on unlink must be treated as already-done: {outcome:?}"
    );
}

#[tokio::test]
async fn batch_partial_failure_three_counts() {
    require_odoo!(url);
    let (db, _d) = setup().await;
    let m = Arc::new(ci_manifest(&url));
    let exec = odoo_executor(Arc::clone(&db), Arc::clone(&m)) as Arc<dyn ConnectorExecutor>;
    let (ctx, _kd) = tool_ctx(Arc::clone(&db)).await;

    // One undoable create (real path), one `final` op (refused → failed), one bad id.
    let create_tool = connector_tool(&m, "odoo_contact_create", Arc::clone(&exec));
    let corr = uuid::Uuid::new_v4().to_string();
    let undoable = run_op_and_row(
        &create_tool,
        &ctx,
        json!({"correlation_ref": corr, "values": {"name": "Batch Eve"}}),
    )
    .await;

    // A `final` op refuses undo (counts as failed) — record it directly (no external write; a
    // final op has no compensation to run through the tool).
    let final_row = journal::insert(
        &db,
        journal::NewAction {
            session_id: &ctx.session_id.to_string(),
            tool_name: "odoo_activity_done",
            tool_tier: "IrreversibleWrite",
            compensability: "final",
            idempotency_key: "batch-final",
            correlation_ref: "corr-final",
            request_params: r#"{"values":{}}"#,
            pre_state: None,
            pre_state_version: None,
            compensation_plan: None,
            retention_days: 30,
        },
    )
    .await
    .unwrap();

    let ids = vec![undoable.id.clone(), final_row.id.clone(), "no-such-id".to_string()];
    let counts = batch_undo(&db, exec.as_ref(), &ids, &ctx.session_id.to_string()).await;
    assert_eq!(counts.undone, 1, "one row undone");
    assert_eq!(counts.failed, 1, "final row refused = failed");
    assert_eq!(counts.not_attempted, 1, "unknown id not attempted");
    assert_eq!(counts.undone + counts.failed + counts.not_attempted, 3, "never one verdict");
}

#[tokio::test]
async fn no_secret_in_journal_row() {
    require_odoo!();
    let (db, _d) = setup().await;
    let (ctx, _kd) = tool_ctx(Arc::clone(&db)).await;
    let m = Arc::new(ci_manifest("https://erp.example.com")); // no live call — journal-only.
    // A never-configured executor is enough: the row is journaled BEFORE the call, so the
    // no-secret assertion can inspect it regardless of whether the call would succeed.
    let exec: Arc<dyn ConnectorExecutor> =
        Arc::new(haily_tools::connector::UnconfiguredExecutor);
    let create_tool = connector_tool(&m, "odoo_contact_create", exec);

    // A poisoned secret in the params proves C4: it must be redacted to the cred REFERENCE.
    let corr = uuid::Uuid::new_v4().to_string();
    let _ = create_tool
        .execute(
            json!({"params": {"correlation_ref": corr, "values": {"name": "Secret Frank"}, "api_key": "sk-MUST-NOT-LEAK"}}),
            &ctx,
        )
        .await; // may Err on the unconfigured call — the outbox row is already written.
    let row = journal::list_by_session(&db, &ctx.session_id.to_string())
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("outbox row written before the call");

    let all = format!(
        "{}{}{}{}",
        row.request_params,
        row.pre_state.clone().unwrap_or_default(),
        row.post_state.clone().unwrap_or_default(),
        row.compensation_plan.clone().unwrap_or_default(),
    );
    assert!(!all.contains("sk-MUST-NOT-LEAK"), "no secret substring in journal: {all}");
    // The actual live key (from the env) also must not appear.
    if let Ok(key) = std::env::var("HAILY_ODOO_API_KEY") {
        if !key.is_empty() {
            assert!(!all.contains(&key), "live key must never reach a journal column");
        }
    }
    assert!(row.request_params.contains(CRED_REF), "credential reference name recorded");
}
