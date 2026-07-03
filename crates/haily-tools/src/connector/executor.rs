//! `ConnectorExecutor` — the external-side seam an action journal write/undo drives.
//!
//! Defined HERE in phase 3 (moved forward from phase 4, C-resequence) purely so
//! `journal_undo` compiles and can be unit-tested against a mock. Phase 4 supplies the
//! real generic HTTP impl and phase 5 the Odoo impl; NO HTTP is implemented here.
//!
//! Contract for the real impl (phase 4):
//! - `call` performs the external write. It re-checks the kill switch just before the
//!   network call (M5 TOCTOU) — that re-check point is documented on `ExecOutcome`.
//! - `read_back` performs a GET by `correlation_ref`/returned id, NEVER a blind retry of
//!   a create (Odoo has no idempotency — M4).
//! - A classified fault is returned as `ExecOutcome::Fault`; a transport/timeout failure
//!   is returned as `Err` so the caller (C7) reads back rather than concluding failure.
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// Outcome of an external write. A `Fault` is a structured error the server RETURNED
/// (has a machine `code` / `data.name`) — distinct from a transport `Err`, which the
/// caller must treat as a lost response (C7) and reconcile via read-back.
#[derive(Debug, Clone)]
pub enum ExecOutcome {
    /// The write succeeded; carries the created/updated record id (if any).
    Ok {
        returned_id: Option<String>,
        body: Value,
    },
    /// The server returned a structured fault. `fault_string` is third-party text
    /// (tag-strip before it reaches an LLM); `code`/`name` are the machine fields the
    /// undo retry logic matches on (e.g. `MissingError` = already-done).
    Fault {
        fault_string: String,
        code: Option<String>,
        name: Option<String>,
    },
}

/// The external connector an action journal write or compensation drives.
///
/// A transport/timeout failure MUST surface as `Err` (not `ExecOutcome::Fault`) so the
/// C7 lost-response path reads back instead of concluding the write failed.
#[async_trait]
pub trait ConnectorExecutor: Send + Sync {
    /// Perform an external write for `op` with `params`.
    ///
    /// # Errors
    /// Returns `Err` on transport/timeout failure (lost response — the caller reads back).
    /// A server-returned structured fault is `Ok(ExecOutcome::Fault)`, not `Err`.
    async fn call(&self, op: &str, params: &Value) -> Result<ExecOutcome>;

    /// Read back the current state of the record identified by `correlation_ref` (or a
    /// returned id embedded in it). Used for post-write verification, reconciliation
    /// (C6), lost-response recovery (C7), and undo idempotency (read-back-before-comp).
    ///
    /// # Errors
    /// Returns `Err` if the read-back GET itself fails — the caller marks the row
    /// `unverified` (does NOT block a later undo).
    async fn read_back(&self, op: &str, correlation_ref: &str) -> Result<Value>;
}

#[cfg(test)]
pub mod mock {
    //! Test double used by `journal_undo` unit tests (phase-3). Scripts a sequence of
    //! `call` outcomes and `read_back` results so retry/refusal paths are exercised
    //! deterministically without any network.
    use super::*;
    use std::sync::Mutex;

    /// A scripted executor. `call_outcomes`/`read_back_results` are consumed front-to-
    /// back per invocation; running past the end yields the LAST scripted item (so a
    /// steady-state can be scripted with one entry). A `read_back` scripted as `Err`
    /// is expressed by pushing `None`.
    pub struct MockExecutor {
        call_outcomes: Mutex<Vec<ExecOutcome>>,
        read_back_results: Mutex<Vec<Option<Value>>>,
        pub calls: Mutex<Vec<String>>,
    }

    impl MockExecutor {
        pub fn new(call_outcomes: Vec<ExecOutcome>, read_back_results: Vec<Option<Value>>) -> Self {
            Self {
                call_outcomes: Mutex::new(call_outcomes),
                read_back_results: Mutex::new(read_back_results),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn pop_call(&self) -> ExecOutcome {
            let mut v = self.call_outcomes.lock().unwrap_or_else(|e| e.into_inner());
            if v.len() > 1 {
                v.remove(0)
            } else {
                v.first().cloned().unwrap_or(ExecOutcome::Ok {
                    returned_id: None,
                    body: Value::Null,
                })
            }
        }

        fn pop_read_back(&self) -> Option<Value> {
            let mut v = self
                .read_back_results
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if v.len() > 1 {
                v.remove(0)
            } else {
                v.first().cloned().flatten()
            }
        }
    }

    #[async_trait]
    impl ConnectorExecutor for MockExecutor {
        async fn call(&self, op: &str, _params: &Value) -> Result<ExecOutcome> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(op.to_string());
            Ok(self.pop_call())
        }

        async fn read_back(&self, _op: &str, _correlation_ref: &str) -> Result<Value> {
            match self.pop_read_back() {
                Some(v) => Ok(v),
                None => anyhow::bail!("mock read-back failure"),
            }
        }
    }
}
