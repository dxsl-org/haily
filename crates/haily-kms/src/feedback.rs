/// Feedback signals and preference adjustment — reachable from both haily-core and haily-tools.
use anyhow::Result;
use haily_db::{queries::meta, DbHandle};
use tracing::info;

#[derive(Debug, Clone)]
pub enum FeedbackSignal {
    Positive,
    Negative { topic: Option<String> },
    Correction { old: String, new: String },
}

/// Persist a preference adjustment derived from the feedback signal.
pub async fn apply_feedback_signal(signal: &FeedbackSignal, db: &DbHandle) -> Result<()> {
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
        }
        FeedbackSignal::Correction { old, new } => {
            let key = format!("feedback.correction.{}", old.replace(' ', "_"));
            meta::upsert_preference(db, &key, new, "explicit").await?;
            info!(%old, %new, "correction saved as preference");
        }
    }
    Ok(())
}
