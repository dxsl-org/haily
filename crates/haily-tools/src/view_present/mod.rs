//! `present_view` — lets the model project data it already has (usually a prior tool result)
//! into a table/cards view for the user to inspect (View Engine Phase A). The model authoring
//! the projection IS the feature here, not a violation: every view this tool mints is stamped
//! `LlmProjected` — badged, quarantined, view-only, and (per the depth-0 guard below) reachable
//! only from the top-level interactive turn, never from inside a delegated sub-agent chain that
//! could feed an autonomous decision. See `schema.rs` (parameters) and `parse.rs`
//! (parse-then-repair into a `DataView`).

mod parse;
mod schema;

use crate::{RiskTier, Tool, ToolContext};
use anyhow::{bail, Result};
use async_trait::async_trait;
use haily_types::ResponseChunk;
use serde_json::Value;

pub struct PresentViewTool;

impl PresentViewTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PresentViewTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for PresentViewTool {
    fn name(&self) -> &str {
        "present_view"
    }

    fn description(&self) -> &str {
        "Trình bày dữ liệu bạn đã có (kết quả tool trước đó, hoặc dữ liệu bạn tự tổng hợp) thành \
         một bảng/thẻ (table/cards) để người dùng xem trực quan trong giao diện. View này CHỈ ĐỂ \
         XEM (read-only) và do AI tự tạo cấu trúc (không tra registry) — không dùng để thu thập \
         input hay xác nhận hành động ghi/xóa. Không gọi tool này từ bên trong một sub-agent \
         được delegate; chỉ dùng ở lượt chat trực tiếp với người dùng."
    }

    fn parameters_schema(&self) -> Value {
        schema::present_view_schema()
    }

    fn risk_tier(&self, _args: &Value) -> RiskTier {
        // View-only: no write path, no journal — a projection is inert display data.
        RiskTier::Read
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        // Quarantine (belt-and-suspenders on top of the sub-turn forwarder, which already
        // discards every non-approval chunk from a delegated turn — see delegate.rs): a
        // projected view must never be mintable from inside an autonomous sub-agent chain
        // that could feed a decision. Only the top-level interactive turn is depth 0.
        if ctx.depth != 0 {
            bail!(
                "present_view is only callable from the top-level interactive turn (depth 0); \
                 refusing at depth {}",
                ctx.depth
            );
        }

        let view = parse::parse_present_view_args(&args)?;
        let entity = view.entity.clone();
        let provenance = view.provenance;
        let record_count = view.records.len();

        let view_id = ctx.view_sink.insert(view);

        // Denominator guard for the Phase-B GO ratio: record this presentation even if the
        // GUI never fetches it. Best-effort — a telemetry failure must never surface as a
        // present_view error (the view is already durably stored regardless).
        if let Err(e) = haily_db::queries::view_events::insert_view_event(
            &ctx.db,
            "presented",
            &view_id.to_string(),
            &ctx.session_id.to_string(),
            None,
        )
        .await
        {
            tracing::warn!(view_id = %view_id, "view_events 'presented' insert failed: {e:#}");
        }

        // Tiny handle on the chat stream — the bulk payload never rides it (Phase 1 contract).
        // Send errors (a closed/cancelled turn) are not this call's failure to report: the view
        // is already durably stored, so a dropped receiver just means nobody was listening.
        let _ = ctx
            .approval_tx
            .send(ResponseChunk::ViewRef {
                view_id,
                entity: entity.clone(),
                provenance,
            })
            .await;

        Ok(format!("Presented a view of {record_count} {entity} records."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::DbHandle;
    use haily_kms::KmsHandle;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    /// Throwaway approval gate — `present_view` is Read-tier and never raises an approval,
    /// but `ToolContext` requires a gate handle (mirrors `skill_fetch.rs`'s test precedent).
    struct NoopGate;
    #[async_trait]
    impl haily_types::ApprovalGate for NoopGate {
        async fn request(&self, _approval_id: Uuid, _session_id: Uuid, _cancel: &CancellationToken) -> bool {
            false
        }
    }

    /// Test-only `ViewSink` that records every inserted view for assertion, mirroring the
    /// no-op sink Phase 1 uses at test construction sites but retaining what was stored.
    struct RecordingViewSink {
        last: Mutex<Option<haily_types::DataView>>,
    }

    impl haily_types::ViewSink for RecordingViewSink {
        fn insert(&self, view: haily_types::DataView) -> Uuid {
            let id = view.view_id;
            *self.last.lock().unwrap() = Some(view);
            id
        }
    }

    fn valid_args() -> Value {
        json!({
            "entity": "contact",
            "schema": [
                { "name": "name", "label": "Name", "ftype": { "type": "Text" } }
            ],
            "records": [
                { "name": "Acme Corp" },
                { "name": "Beta LLC" }
            ]
        })
    }

    async fn test_ctx(
        dir: &std::path::Path,
        depth: u8,
    ) -> (ToolContext, tokio::sync::mpsc::Receiver<ResponseChunk>) {
        let db = Arc::new(DbHandle::init(&dir.join("t.db")).await.unwrap());
        let kms = Arc::new(KmsHandle::init((*db).clone(), dir).await.unwrap());
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let ctx = ToolContext {
            db,
            kms,
            session_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            depth,
            domain: None,
            approval_gate: Arc::new(NoopGate),
            approval_tx: tx,
            cancel: CancellationToken::new(),
            turn_deletes: Arc::new(AtomicUsize::new(0)),
            last_journal_id: Arc::new(Mutex::new(None)),
            run_id: None,
            depth_mode: haily_types::DepthMode::Normal,
            view_sink: Arc::new(RecordingViewSink {
                last: Mutex::new(None),
            }),
        };
        (ctx, rx)
    }

    #[tokio::test]
    async fn valid_projection_stores_llm_projected_view_and_emits_one_view_ref() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, mut rx) = test_ctx(dir.path(), 0).await;
        let tool = PresentViewTool::new();
        let out = tool.execute(valid_args(), &ctx).await.expect("valid call must succeed");
        assert_eq!(out, "Presented a view of 2 contact records.");

        let chunk = rx.recv().await.expect("must emit exactly one chunk");
        match chunk {
            ResponseChunk::ViewRef { entity, provenance, .. } => {
                assert_eq!(entity, "contact");
                assert_eq!(provenance, haily_types::ViewProvenance::LlmProjected);
            }
            other => panic!("expected ViewRef, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "must emit exactly one ViewRef chunk, not more"
        );
    }

    #[tokio::test]
    async fn malformed_args_return_err_without_inserting_a_view() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _rx) = test_ctx(dir.path(), 0).await;
        let tool = PresentViewTool::new();
        let bad = json!({ "nothing": "recognizable" });
        assert!(tool.execute(bad, &ctx).await.is_err());
    }

    #[tokio::test]
    async fn refuses_when_called_from_a_delegated_depth() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _rx) = test_ctx(dir.path(), 1).await;
        let tool = PresentViewTool::new();
        let err = tool
            .execute(valid_args(), &ctx)
            .await
            .expect_err("depth != 0 must be refused");
        assert!(err.to_string().contains("depth 0"));
    }
}
