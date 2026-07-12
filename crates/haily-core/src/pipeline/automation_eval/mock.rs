//! In-process, ZERO-NETWORK mock SaaS server for the automation/connector eval
//! (Sub-Agent + Skill Architecture phase 14).
//!
//! Speaks the EXACT Odoo `execute_kw` JSON-RPC dialect Haily's shipped `odoo-crm` manifest
//! produces (see `connectors/odoo-crm.manifest.json` + `tests/fixtures/odoo_wire_fixtures.rs`),
//! so the generic `HttpConnectorTool` interprets a manifest against it as a DROP-IN target —
//! no connector code changes (LOCKED decision 4). Faithful enough that protocol translation,
//! read-back, and fault classification match a real endpoint (the phase's explicit
//! false-confidence risk note): create returns the new id, write/unlink return `true`,
//! `search_read` returns a one-element record list, and a server fault is the Odoo
//! `error.data.name` shape the manifest's `fault_rules` key on.
//!
//! Determinism: `write_date` is a monotonic `"v{n}"` counter (never a wall clock), so an
//! identical task run yields a byte-identical end-state — the reproducibility the eval's
//! bit-equal undo check and the scripted golden both depend on. Bound to `127.0.0.1:0`
//! (loopback), reached only via the connector executor's TEST-ONLY `allow_loopback`.
//!
//! The store is shared (`Arc<Mutex<_>>`) between the spawned HTTP task and the runner, so the
//! runner scores objective/guardrail assertions by a DIRECT in-memory read of the final state
//! (the writes still travelled the real connector network path; only the scoring read is
//! in-process) and reseeds deterministically per task.
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One seeded record: its model, its correlation `ref` (the field Odoo CRM models carry for
/// C7), and its initial fields. The mock assigns the server-side integer id.
#[derive(Clone, Debug)]
pub struct SeedRecord {
    pub model: String,
    pub reference: String,
    pub fields: Value,
}

/// The mock's in-memory state: per-model record tables keyed by server-assigned id, plus the
/// monotonic id + version counters. `active` defaults to `true` on create (Odoo semantics).
#[derive(Default)]
pub struct MockState {
    models: BTreeMap<String, BTreeMap<i64, Map<String, Value>>>,
    next_id: i64,
    version: u64,
}

impl MockState {
    fn seed(records: &[SeedRecord]) -> Self {
        let mut s = MockState {
            next_id: 1000,
            version: 0,
            ..Default::default()
        };
        for r in records {
            let id = s.next_id;
            s.next_id += 1;
            s.version += 1;
            let mut rec = r.fields.as_object().cloned().unwrap_or_default();
            rec.insert("id".to_string(), json!(id));
            rec.insert("ref".to_string(), json!(r.reference));
            rec.entry("active").or_insert(json!(true));
            rec.insert("write_date".to_string(), json!(format!("v{}", s.version)));
            s.models.entry(r.model.clone()).or_default().insert(id, rec);
        }
        s
    }

    fn bump(&mut self) -> String {
        self.version += 1;
        format!("v{}", self.version)
    }

    /// Every record of `model` whose `field` equals `value` (used by the eval's deterministic
    /// objective/guardrail assertions — a direct read, never a network call).
    pub fn find(&self, model: &str, field: &str, value: &Value) -> Vec<Map<String, Value>> {
        self.models
            .get(model)
            .map(|tbl| {
                tbl.values()
                    .filter(|rec| rec.get(field) == Some(value))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A deterministic, order-independent digest of the whole store — the eval compares this
    /// before a run's writes and after `undo_turn` to assert the seed state is restored
    /// BIT-EQUAL. `write_date` is EXCLUDED: an undo legitimately bumps it (a fresh write), so
    /// the field-level restoration — not the version counter — is what "bit-equal" means here.
    pub fn digest(&self) -> String {
        let mut out = String::new();
        for (model, tbl) in &self.models {
            for (id, rec) in tbl {
                out.push_str(model);
                out.push('#');
                out.push_str(&id.to_string());
                let mut keys: Vec<&String> = rec.keys().filter(|k| *k != "write_date").collect();
                keys.sort();
                for k in keys {
                    out.push('|');
                    out.push_str(k);
                    out.push('=');
                    out.push_str(&rec.get(k).map(Value::to_string).unwrap_or_default());
                }
                out.push('\n');
            }
        }
        out
    }
}

/// A running mock SaaS instance: the base URL its manifest points at + the shared store.
pub struct MockSaas {
    pub base_url: String,
    pub state: Arc<Mutex<MockState>>,
}

impl MockSaas {
    /// Start the loopback server seeded with `records`, returning its base URL + shared store.
    /// The server task lives for the process; a dropped `MockSaas` just stops new resets.
    pub async fn start(records: Vec<SeedRecord>) -> Self {
        let state = Arc::new(Mutex::new(MockState::seed(&records)));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock saas");
        let addr = listener.local_addr().expect("mock addr");
        let srv_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let st = Arc::clone(&srv_state);
                tokio::spawn(handle_conn(stream, st));
            }
        });
        MockSaas {
            base_url: format!("http://{addr}"),
            state,
        }
    }

    /// Reseed the store deterministically before a task run (per-task seed reset).
    pub fn reset(&self, records: Vec<SeedRecord>) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *guard = MockState::seed(&records);
    }

    /// A direct snapshot digest for the bit-equal undo assertion.
    pub fn digest(&self) -> String {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).digest()
    }
}

async fn handle_conn(mut stream: tokio::net::TcpStream, state: Arc<Mutex<MockState>>) {
    let body = match read_http_body(&mut stream).await {
        Some(b) => b,
        None => return,
    };
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let result = handle_rpc(&req, &state);
    let payload = match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": null, "result": v }).to_string(),
        Err((name, message)) => json!({
            "jsonrpc": "2.0", "id": null,
            "error": { "code": 200, "message": message, "data": { "name": name, "message": message } }
        })
        .to_string(),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Read a full HTTP request body off the socket, honoring `Content-Length` (the connector
/// always sends one) so a body split across TCP reads is reassembled before parsing.
async fn read_http_body(stream: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n");
        if let Some(pos) = header_end {
            let header = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
            let content_len = header
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = pos + 4;
            if buf.len() >= body_start + content_len {
                return Some(buf[body_start..body_start + content_len].to_vec());
            }
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

type RpcResult = Result<Value, (String, String)>;

/// Interpret one `execute_kw` envelope against the store, returning the Odoo-shaped `result`
/// or a `(fault_name, message)` the response wraps as `error.data.name`.
fn handle_rpc(req: &Value, state: &Arc<Mutex<MockState>>) -> RpcResult {
    let args = req.pointer("/params/args").and_then(Value::as_array).ok_or_else(|| {
        ("odoo.exceptions.ValidationError".to_string(), "malformed execute_kw".to_string())
    })?;
    // args = [db, uid, key, model, method, args, kwargs]
    let model = args.get(3).and_then(Value::as_str).unwrap_or_default().to_string();
    let method = args.get(4).and_then(Value::as_str).unwrap_or_default().to_string();
    let call_args = args.get(5).cloned().unwrap_or(Value::Null);
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());

    match method.as_str() {
        "create" => {
            let vals = call_args.get(0).cloned().unwrap_or(Value::Null);
            let mut rec = vals.as_object().cloned().ok_or_else(|| {
                ("odoo.exceptions.ValidationError".to_string(), "create needs a vals object".to_string())
            })?;
            let id = st.next_id;
            st.next_id += 1;
            let v = st.bump();
            rec.insert("id".to_string(), json!(id));
            rec.entry("active").or_insert(json!(true));
            rec.insert("write_date".to_string(), json!(v));
            st.models.entry(model).or_default().insert(id, rec);
            Ok(json!(id))
        }
        "write" => {
            let ids = ids_arg(call_args.get(0));
            let values = call_args.get(1).and_then(Value::as_object).cloned().unwrap_or_default();
            let v = st.bump();
            let tbl = st.models.entry(model).or_default();
            for id in &ids {
                if let Some(rec) = tbl.get_mut(id) {
                    for (k, val) in &values {
                        rec.insert(k.clone(), val.clone());
                    }
                    rec.insert("write_date".to_string(), json!(v));
                }
            }
            Ok(json!(true))
        }
        "unlink" => {
            let ids = ids_arg(call_args.get(0));
            let tbl = st.models.entry(model).or_default();
            for id in &ids {
                tbl.remove(id);
            }
            Ok(json!(true))
        }
        "search_read" => {
            let domain = call_args.get(0).cloned().unwrap_or(Value::Null);
            let found = search(&st, &model, &domain);
            Ok(Value::Array(found))
        }
        // A model-side action (e.g. mail.activity.action_feedback / a `final` op): no state
        // change to model — return truthy so the connector reads it as a landed write.
        other => {
            let _ = other;
            Ok(json!(true))
        }
    }
}

/// Extract an id list from a write/unlink first arg: `[[ids]]`'s inner list, or a bare scalar.
fn ids_arg(arg: Option<&Value>) -> Vec<i64> {
    match arg {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .collect(),
        Some(v) => v.as_i64().into_iter().collect(),
        None => Vec::new(),
    }
}

/// Filter a model's records by a `search_read` domain of `["field","=",value]` clauses (the
/// only operator the connector ever emits for read-back). Returns matching records (INCLUDING
/// inactive ones, so an archive's read-back verification still locates the record).
fn search(st: &MockState, model: &str, domain: &Value) -> Vec<Value> {
    let Some(tbl) = st.models.get(model) else { return Vec::new() };
    // `domain` here is the connector's `search_read` domain — a list of `["field","=",val]`
    // clauses (the caller already peeled the execute_kw args wrapper). An empty/absent domain
    // matches every record (Odoo semantics).
    let clauses = domain.as_array().cloned().unwrap_or_default();
    tbl.values()
        .filter(|rec| clauses.iter().all(|c| clause_matches(rec, c)))
        .map(|rec| Value::Object(rec.clone()))
        .collect()
}

fn clause_matches(rec: &Map<String, Value>, clause: &Value) -> bool {
    let Some(c) = clause.as_array() else { return true };
    let (Some(field), Some(op), Some(val)) = (c.first().and_then(Value::as_str), c.get(1).and_then(Value::as_str), c.get(2))
    else {
        return true;
    };
    let actual = rec.get(field).unwrap_or(&Value::Null);
    match op {
        "=" => values_eq(actual, val),
        "!=" => !values_eq(actual, val),
        _ => true,
    }
}

/// Loose scalar equality tolerant of the int-vs-string id representation the domain may carry
/// (`["id","=","1000"]` vs a stored integer 1000).
fn values_eq(a: &Value, b: &Value) -> bool {
    if a == b {
        return true;
    }
    match (a.as_i64(), b.as_i64()) {
        (Some(x), Some(y)) => x == y,
        _ => a.as_str().zip(b.as_str()).map(|(x, y)| x == y).unwrap_or(false),
    }
}
