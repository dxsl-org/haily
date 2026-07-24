//! RunEvent persistence (Unified Chat UI phase 5, D2). One row per non-`StageOutput` event a
//! pipeline run emits, insertion-ordered by `id` (see migration 0034 for why `id`, not
//! `created_at`). `StageOutput`'s raw `chunk` text is never persisted here — its only trace is
//! a text-free `(run_id, stage)` count/last-seq marker (credential-leak fix, phase 05 Security
//! Considerations).
use crate::DbHandle;
use anyhow::{Context, Result};
use haily_types::RunEvent;
use sqlx::FromRow;

/// Extract the owning run id from any `RunEvent` variant. Deliberately exhaustive (no `_ =>`
/// arm) — a future variant added to `haily-types` fails this match at compile time instead of
/// silently losing its `run_id` at persistence time.
pub fn run_id_of(event: &RunEvent) -> &str {
    match event {
        RunEvent::RunStarted { run_id, .. }
        | RunEvent::StageStarted { run_id, .. }
        | RunEvent::StageOutput { run_id, .. }
        | RunEvent::GateResult { run_id, .. }
        | RunEvent::Retry { run_id, .. }
        | RunEvent::Escalation { run_id, .. }
        | RunEvent::DiffAvailable { run_id, .. }
        | RunEvent::ApprovalNeeded { run_id, .. }
        | RunEvent::PlanReady { run_id, .. }
        | RunEvent::RunPaused { run_id, .. }
        | RunEvent::RunComplete { run_id, .. } => run_id,
    }
}

/// The `kind` column value for one `RunEvent` variant — mirrors serde's own `tag` name so a raw
/// SQL read can filter/debug without deserializing `payload`. Exhaustive for the same reason as
/// [`run_id_of`].
fn event_kind(event: &RunEvent) -> &'static str {
    match event {
        RunEvent::RunStarted { .. } => "RunStarted",
        RunEvent::StageStarted { .. } => "StageStarted",
        RunEvent::StageOutput { .. } => "StageOutput",
        RunEvent::GateResult { .. } => "GateResult",
        RunEvent::Retry { .. } => "Retry",
        RunEvent::Escalation { .. } => "Escalation",
        RunEvent::DiffAvailable { .. } => "DiffAvailable",
        RunEvent::ApprovalNeeded { .. } => "ApprovalNeeded",
        RunEvent::PlanReady { .. } => "PlanReady",
        RunEvent::RunPaused { .. } => "RunPaused",
        RunEvent::RunComplete { .. } => "RunComplete",
    }
}

/// Persist one non-`StageOutput` `RunEvent` as a full JSON row. Called by the per-run event
/// bridge AFTER delivery to the live adapter (best-effort — a caller propagates the error only
/// as a logged warning, never a delivery stall).
///
/// A `StageOutput` event is silently dropped (no row, no error) rather than persisted — the
/// bridge already routes it to [`upsert_stage_marker`] instead; this is a defense-in-depth
/// backstop so a future caller mistake can never leak `chunk` text into this table.
///
/// `GateResult.decisive` is redacted to an empty string before this row is written — it is raw
/// verifier output (a failing test/compiler line can echo a secret), the same leak class the
/// `StageOutput` exclusion above closes, and this table's backup scrub is a keyed row-delete
/// that cannot scan payload content. The live in-memory delivery above (before this call) still
/// carries the full `decisive` text to the GUI/CLI/Telegram surface; only the persisted copy
/// drops it, keeping gate identity + pass/fail for replay.
///
/// # Errors
/// Returns an error if serialization or the insert fails.
pub async fn insert_run_event(db: &DbHandle, run_id: &str, event: &RunEvent) -> Result<()> {
    if matches!(event, RunEvent::StageOutput { .. }) {
        return Ok(());
    }
    let redacted;
    let to_persist: &RunEvent = if let RunEvent::GateResult {
        run_id, gate, pass, ..
    } = event
    {
        redacted = RunEvent::GateResult {
            run_id: run_id.clone(),
            gate: gate.clone(),
            pass: *pass,
            decisive: String::new(),
        };
        &redacted
    } else {
        event
    };
    let payload =
        serde_json::to_string(to_persist).context("serialize RunEvent for persistence")?;
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO run_events (run_id, kind, payload, created_at) VALUES (?, ?, ?, ?)")
        .bind(run_id)
        .bind(event_kind(event))
        .bind(payload)
        .bind(&now)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Upsert a `(run_id, stage)` marker on a `StageOutput` chunk: bumps `count`, records the
/// chunk's `seq` as `last_seq`. Never stores the chunk's text.
///
/// # Errors
/// Returns an error if the upsert fails.
pub async fn upsert_stage_marker(db: &DbHandle, run_id: &str, stage: &str, seq: u64) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO run_stage_marker (run_id, stage, count, last_seq, updated_at)
         VALUES (?, ?, 1, ?, ?)
         ON CONFLICT(run_id, stage) DO UPDATE SET
             count = count + 1,
             last_seq = excluded.last_seq,
             updated_at = excluded.updated_at",
    )
    .bind(run_id)
    .bind(stage)
    .bind(seq as i64)
    .bind(&now)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// One `run_stage_marker` row — a text-free preview of a stage's `StageOutput` volume (P07
/// consumes this alongside [`list_run_events`] for a run's detail view).
#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct StageMarker {
    pub run_id: String,
    pub stage: String,
    pub count: i64,
    pub last_seq: i64,
    pub updated_at: String,
}

/// List every stage marker for a run, in first-updated order.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn list_stage_markers(db: &DbHandle, run_id: &str) -> Result<Vec<StageMarker>> {
    Ok(sqlx::query_as::<_, StageMarker>(
        "SELECT run_id, stage, count, last_seq, updated_at
         FROM run_stage_marker WHERE run_id = ? ORDER BY updated_at ASC",
    )
    .bind(run_id)
    .fetch_all(db.pool())
    .await?)
}

/// Rehydrate a run's persisted timeline, oldest first (`ORDER BY id`, the AUTOINCREMENT
/// insertion-order key — see migration 0034 for why not `created_at`).
///
/// Persistence is best-effort (bridge writes AFTER delivery), so a crash between delivering and
/// persisting the terminal event can leave no `RunComplete` row even though the run is actually
/// done. Reconciled here against `pipeline_runs` (the authoritative status source, MAJOR
/// red-team finding P05/P07): if the persisted rows carry no `RunComplete` and the run's own
/// row is in a terminal-or-interrupted state, a `RunComplete` is synthesized from that status so
/// a caller never has to cross-reference the two tables itself.
///
/// # Errors
/// Returns an error if either query, or deserializing a persisted payload, fails.
pub async fn list_run_events(db: &DbHandle, run_id: &str) -> Result<Vec<RunEvent>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT payload FROM run_events WHERE run_id = ? ORDER BY id ASC")
            .bind(run_id)
            .fetch_all(db.pool())
            .await?;

    let mut events = Vec::with_capacity(rows.len());
    let mut has_terminal = false;
    for (payload,) in rows {
        let event: RunEvent = serde_json::from_str(&payload)
            .with_context(|| format!("deserialize persisted RunEvent for run {run_id}"))?;
        if matches!(event, RunEvent::RunComplete { .. }) {
            has_terminal = true;
        }
        events.push(event);
    }

    if !has_terminal {
        if let Some(status) = crate::queries::pipeline_runs::status_of(db, run_id).await? {
            if matches!(status.as_str(), "done" | "failed" | "interrupted") {
                events.push(RunEvent::RunComplete {
                    run_id: run_id.to_string(),
                    outcome: status,
                });
            }
        }
    }

    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::{pipeline_runs, sessions};
    use uuid::Uuid;

    async fn setup() -> (DbHandle, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        let session_id = Uuid::new_v4().to_string();
        sessions::create_session(&db, &session_id, "test", None)
            .await
            .unwrap();
        let run = pipeline_runs::create(&db, &session_id, None, 4)
            .await
            .unwrap();
        (db, run.id, dir)
    }

    #[tokio::test]
    async fn round_trips_and_preserves_insertion_order() {
        let (db, run_id, _dir) = setup().await;
        let events = vec![
            RunEvent::RunStarted {
                run_id: run_id.clone(),
                work_item_id: "w1".into(),
            },
            RunEvent::StageStarted {
                run_id: run_id.clone(),
                stage: "build".into(),
                tier: None,
            },
            RunEvent::GateResult {
                run_id: run_id.clone(),
                gate: "command".into(),
                pass: true,
                decisive: String::new(),
            },
            RunEvent::RunComplete {
                run_id: run_id.clone(),
                outcome: "done".into(),
            },
        ];
        for ev in &events {
            insert_run_event(&db, &run_id, ev).await.unwrap();
        }

        let replayed = list_run_events(&db, &run_id).await.unwrap();
        assert_eq!(
            replayed, events,
            "must round-trip in exact insertion order via id"
        );
    }

    /// `StageOutput` never becomes a row — only a text-free marker — even if a caller passes it
    /// straight to `insert_run_event` (the credential-leak backstop).
    #[tokio::test]
    async fn stage_output_creates_no_row_and_no_text() {
        let (db, run_id, _dir) = setup().await;
        let chunk = RunEvent::StageOutput {
            run_id: run_id.clone(),
            seq: 0,
            chunk: "SECRET=abc".into(),
        };
        insert_run_event(&db, &run_id, &chunk).await.unwrap();

        let replayed = list_run_events(&db, &run_id).await.unwrap();
        assert!(
            replayed.is_empty(),
            "StageOutput must never persist as a run_events row"
        );

        upsert_stage_marker(&db, &run_id, "build", 3).await.unwrap();
        upsert_stage_marker(&db, &run_id, "build", 4).await.unwrap();
        let markers = list_stage_markers(&db, &run_id).await.unwrap();
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].count, 2, "count bumps on each chunk");
        assert_eq!(
            markers[0].last_seq, 4,
            "last_seq tracks the most recent chunk"
        );
    }

    /// `GateResult.decisive` is raw verifier output (same leak class as `StageOutput.chunk`) —
    /// the persisted row must never carry it, even though the in-memory event handed to
    /// `insert_run_event` does. Checks both the replayed value AND the raw stored payload text,
    /// so a future refactor can't satisfy the struct-level assertion while still leaking the
    /// secret into an untyped column.
    #[tokio::test]
    async fn gate_result_decisive_text_is_redacted_before_persist() {
        let (db, run_id, _dir) = setup().await;
        let secret = "SECRET_TOKEN=abc123 test failed";
        insert_run_event(
            &db,
            &run_id,
            &RunEvent::GateResult {
                run_id: run_id.clone(),
                gate: "command".into(),
                pass: false,
                decisive: secret.into(),
            },
        )
        .await
        .unwrap();

        let replayed = list_run_events(&db, &run_id).await.unwrap();
        assert_eq!(
            replayed,
            vec![RunEvent::GateResult {
                run_id: run_id.clone(),
                gate: "command".into(),
                pass: false,
                decisive: String::new(),
            }],
            "gate identity + pass/fail survive; decisive text is dropped"
        );

        let (raw_payload,): (String,) =
            sqlx::query_as("SELECT payload FROM run_events WHERE run_id = ?")
                .bind(&run_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert!(
            !raw_payload.contains(secret),
            "raw stored payload must not contain the secret text either"
        );
    }

    /// A run whose row is terminal but never got a persisted `RunComplete` (crash between
    /// delivery and persistence) still reconciles to a synthesized terminal marker on read.
    #[tokio::test]
    async fn synthesizes_terminal_marker_when_run_row_is_terminal_but_event_missing() {
        let (db, run_id, _dir) = setup().await;
        insert_run_event(
            &db,
            &run_id,
            &RunEvent::StageStarted {
                run_id: run_id.clone(),
                stage: "build".into(),
                tier: None,
            },
        )
        .await
        .unwrap();

        pipeline_runs::transition(
            &db,
            &run_id,
            pipeline_runs::RunTransition {
                stage_index: 0,
                status: "failed",
                attempt: 1,
                attempts_remaining: 3,
                tier_used: None,
                backend_used: None,
                egress: None,
                gate_output_digest: None,
                pause_reason_class: None,
            },
        )
        .await
        .unwrap();

        let replayed = list_run_events(&db, &run_id).await.unwrap();
        assert_eq!(
            replayed.len(),
            2,
            "one real row + one synthesized terminal marker"
        );
        assert_eq!(
            replayed[1],
            RunEvent::RunComplete {
                run_id: run_id.clone(),
                outcome: "failed".into()
            }
        );
    }
}
