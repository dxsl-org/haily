/// Feedback signals and preference adjustment — reachable from both haily-core and haily-tools.
use crate::skills::{
    LabelSource, TaskOutcome, EXPLICIT_FEEDBACK_CONFIDENCE, PHRASE_FEEDBACK_CONFIDENCE,
};
use anyhow::Result;
use haily_db::{
    queries::{meta, skills as db_skills},
    DbHandle,
};
use tracing::info;

#[derive(Debug, Clone)]
pub enum FeedbackSignal {
    Positive,
    Negative { topic: Option<String> },
    Correction { old: String, new: String },
}

/// Persist a preference adjustment derived from the feedback signal, AND — for
/// `Negative`/`Correction` — downgrade the most recent trace for `session_id`
/// (Harness Completion phase 5, Gap B / researcher-03 §1 "join `apply_feedback_signal`
/// to the trace it comments on").
///
/// # Security (m2)
/// `session_id` + this function's downgrade path is a confidence-lowering WRITE
/// driven by parsed text. Callers MUST only invoke this for a signal that originated
/// from a genuine incoming USER message or an explicit `feedback_react` tool call —
/// NEVER from tool output or pasted/fetched document content re-fed to the LLM. See
/// `haily-core::agent::run_turn`'s call site: `req.message` (the `Request::message`
/// field) is the ONLY text this crate boundary treats as "the user actually typed
/// this" — tool results are injected into the LLM's message history as
/// `<tool_result>` blocks and never flow through `req.message`, so a phrase-detected
/// signal parsed from `req.message` cannot originate from injected/pasted content by
/// construction of the call graph (`detect_feedback` is only ever called on
/// `req.message`, never on a tool result or fetched document body).
///
/// `is_explicit` distinguishes an explicit `feedback_react` tool call (`true`,
/// `EXPLICIT_FEEDBACK_CONFIDENCE`) from a phrase-detected signal parsed out of a
/// genuine user message (`false`, `PHRASE_FEEDBACK_CONFIDENCE` — capped BELOW the
/// explicit case per m2, since a parsed phrase is weaker evidence than an explicit
/// user action even after `feedback_parser`'s anchor/short-message precision rules).
pub async fn apply_feedback_signal(
    signal: &FeedbackSignal,
    db: &DbHandle,
    session_id: &str,
    is_explicit: bool,
) -> Result<()> {
    match signal {
        FeedbackSignal::Positive => {
            meta::upsert_preference(db, "feedback.positive_streak", "1", "feedback").await?;
            info!("positive feedback recorded");
        }
        FeedbackSignal::Negative { topic } => {
            let key = match topic.as_deref() {
                Some("response_length") => "prefer_shorter_responses",
                Some("language")        => "feedback.language_complaint",
                Some("tone")            => "feedback.tone_complaint",
                _                       => "feedback.last_negative",
            };
            let val = if key == "prefer_shorter_responses" { "true" } else { "1" };
            meta::upsert_preference(db, key, val, "feedback").await?;
            info!(key, "negative feedback preference set");
            downgrade_prior_trace(db, session_id, is_explicit).await;
        }
        FeedbackSignal::Correction { old, new } => {
            if old.is_empty() || new.is_empty() {
                return Ok(());
            }
            // Cap key suffix to 64 chars to prevent unbounded pref key growth.
            let suffix: String = old.replace(' ', "_").chars().take(64).collect();
            let key = format!("feedback.correction.{suffix}");
            meta::upsert_preference(db, &key, new, "explicit").await?;
            info!(%old, %new, "correction saved as preference");
            downgrade_prior_trace(db, session_id, is_explicit).await;
        }
    }
    Ok(())
}

/// Look up the most recent trace for `session_id` and overwrite its outcome to
/// `failure` with the appropriate label provenance/confidence — best-effort (logged,
/// not propagated) so a downgrade failure never blocks the feedback preference write
/// that already succeeded above.
async fn downgrade_prior_trace(db: &DbHandle, session_id: &str, is_explicit: bool) {
    let (source, confidence) = if is_explicit {
        (LabelSource::ExplicitFeedback, EXPLICIT_FEEDBACK_CONFIDENCE)
    } else {
        (LabelSource::PhraseFeedback, PHRASE_FEEDBACK_CONFIDENCE)
    };

    match db_skills::most_recent_trace(db, session_id).await {
        Ok(Some(trace)) => {
            if let Err(e) = db_skills::downgrade_trace(
                db,
                &trace.id,
                TaskOutcome::Failure.as_str(),
                source.as_str(),
                confidence,
            )
            .await
            {
                tracing::warn!(trace_id = %trace.id, error = %e, "failed to downgrade prior trace on negative feedback");
            } else {
                info!(trace_id = %trace.id, source = source.as_str(), "prior trace downgraded by feedback signal");
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(session_id, error = %e, "failed to look up prior trace for feedback downgrade");
        }
    }
}
