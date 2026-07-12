//! DB-backed session transcript provider for the ACP channel's `session/load` replay
//! (Sub-Agent + Skill Architecture phase 12).
//!
//! Lives at the app layer — not in `haily-io` — because reading persisted messages needs
//! `haily-db`, which the leaf `haily-io` adapter crate must not depend on (the CLAUDE.md
//! layering invariant). It is injected post-construction via `Adapter::set_session_transcript`,
//! exactly like the approval resolver and kill switch, so the ACP adapter maps its sessions
//! onto Haily's EXISTING `sessions`/`messages` storage rather than keeping a parallel copy.

use async_trait::async_trait;
use haily_db::{queries::sessions, DbHandle};
use haily_types::{SessionTranscript, TranscriptEntry};
use std::sync::Arc;

/// Upper bound on how many messages are replayed on a `session/load`. A long-lived session's
/// full history is not needed to rebuild an editor's view — the most recent window is, and it
/// caps the burst of `session/update` frames sent before the load resolves.
const REPLAY_LIMIT: i64 = 200;

/// Reads a session's message history from the `messages` table.
pub struct DbSessionTranscript {
    db: Arc<DbHandle>,
}

impl DbSessionTranscript {
    pub fn new(db: Arc<DbHandle>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl SessionTranscript for DbSessionTranscript {
    /// Return up to [`REPLAY_LIMIT`] most-recent messages in chronological order (oldest
    /// first — `sessions::recent_messages`'s contract). Best-effort: a DB error logs and
    /// yields an empty transcript so `session/load` still resolves (replay is UX, never a
    /// correctness gate).
    async fn transcript(&self, session_id: &str) -> Vec<TranscriptEntry> {
        match sessions::recent_messages(&self.db, session_id, REPLAY_LIMIT).await {
            Ok(msgs) => msgs
                .into_iter()
                .map(|m| TranscriptEntry { role: m.role, content: m.content })
                .collect(),
            Err(e) => {
                tracing::warn!("acp: transcript load for session {session_id} failed: {e:#}");
                Vec::new()
            }
        }
    }
}
