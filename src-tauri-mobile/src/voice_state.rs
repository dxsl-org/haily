//! m4 push-to-talk contract (Mobile Thin-Client plan phase 4, red team m4): starting STT MUST
//! stop active TTS AND pause the chunker feed, so the mic can never capture the assistant's own
//! spoken output; resuming after the STT session ends re-enables the chunker. This module is the
//! pure, host-testable decision core — it holds no Tauri/plugin handle and performs no I/O itself;
//! `voice.rs`'s Tauri commands hold one [`PushToTalkGate`] in `AppState` and execute whatever
//! [`GateAction`]s each transition returns.
//!
//! The realistic scenario this guards against is NOT mid-turn barge-in (the mobile UI already
//! disables push-to-talk while a turn is in flight, mirroring the text input's own
//! `activeSession` gate) — it's the trailing-audio race: the assistant's TEXT stream can finish
//! (unblocking the UI) well before its slower TTS audio queue finishes playing the last few
//! queued sentences. If the user then presses push-to-talk to ask a follow-up, the mic must not
//! pick up that still-playing trailing audio.

/// The gate's two observable states. `Listening` covers the ENTIRE window from press to a
/// confirmed STT outcome (final transcript, error, or cancellation) — not just the physical
/// press-to-release window — because Android's `SpeechRecognizer` keeps processing buffered
/// audio for a short time after `stopListening()` is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceState {
    Idle,
    Listening,
}

/// A side effect the command layer must perform in response to a gate transition. Kept as data
/// (not executed inline) so the gate itself stays a pure, allocation-free state machine that
/// unit tests can drive without any Tauri/plugin/async machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    /// Flush-stop the TTS synthesizer immediately (`stop_speaking`, `QUEUE_FLUSH` semantics).
    /// Always emitted on press regardless of whether anything was actually playing — cheaper and
    /// safer than trying to track "was TTS really speaking" here (mirrors this codebase's
    /// existing "always gate, never under-cover" precedent for the biometric approval check).
    StopSpeaking,
    /// Mute the `haily-chunk` → chunker → `speak_chunk` wiring so no NEW sentence starts queuing
    /// while the mic is live.
    PauseChunkerFeed,
    /// Unmute that wiring — the STT session that required the pause has fully ended.
    ResumeChunkerFeed,
}

/// The push-to-talk gate: press → stop TTS + pause the chunker feed; the feed stays paused for
/// the ENTIRE STT session (through release, through Android's post-`stopListening()` processing
/// tail) and only resumes once a final/error/cancel outcome is observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushToTalkGate {
    state: VoiceState,
}

impl Default for PushToTalkGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PushToTalkGate {
    pub fn new() -> Self {
        Self {
            state: VoiceState::Idle,
        }
    }

    pub fn state(&self) -> VoiceState {
        self.state
    }

    pub fn is_chunker_paused(&self) -> bool {
        self.state == VoiceState::Listening
    }

    /// Push-to-talk pressed. Idempotent: a stray double-press while already `Listening` returns
    /// no actions (must not re-flush TTS or re-emit a redundant pause every repeat tap/bounce).
    pub fn on_ptt_pressed(&mut self) -> Vec<GateAction> {
        if self.state == VoiceState::Listening {
            return Vec::new();
        }
        self.state = VoiceState::Listening;
        vec![GateAction::StopSpeaking, GateAction::PauseChunkerFeed]
    }

    /// Push-to-talk released. Deliberately a NO-OP on the gate's own state — release only asks
    /// the recognizer to finalize; the mic may still be capturing its processing tail, so the
    /// chunker feed must stay paused until [`Self::on_stt_ended`] fires. This method exists as an
    /// explicit call site (rather than folding release into "nothing happens") so the 3-moment
    /// contract — press / release / finalize — stays legible at the call site and in tests,
    /// instead of silently relying on the caller to know release does nothing here.
    pub fn on_ptt_released(&mut self) {}

    /// The STT session ended — with a final transcript, a recognizer error, or an explicit
    /// cancel. ALWAYS resumes on any of these (never leaves TTS muted because of a failed/aborted
    /// recognition); idempotent if the gate was already `Idle` (e.g. a stray duplicate event).
    pub fn on_stt_ended(&mut self) -> Vec<GateAction> {
        if self.state == VoiceState::Idle {
            return Vec::new();
        }
        self.state = VoiceState::Idle;
        vec![GateAction::ResumeChunkerFeed]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_idle_and_unpaused() {
        let gate = PushToTalkGate::new();
        assert_eq!(gate.state(), VoiceState::Idle);
        assert!(!gate.is_chunker_paused());
    }

    #[test]
    fn press_stops_tts_and_pauses_the_chunker_feed() {
        let mut gate = PushToTalkGate::new();
        let actions = gate.on_ptt_pressed();
        assert_eq!(
            actions,
            vec![GateAction::StopSpeaking, GateAction::PauseChunkerFeed]
        );
        assert_eq!(gate.state(), VoiceState::Listening);
        assert!(gate.is_chunker_paused());
    }

    #[test]
    fn a_repeated_press_while_already_listening_is_a_no_op() {
        let mut gate = PushToTalkGate::new();
        gate.on_ptt_pressed();
        let second = gate.on_ptt_pressed();
        assert!(
            second.is_empty(),
            "must not re-flush TTS or re-pause on a stray double-press"
        );
        assert_eq!(gate.state(), VoiceState::Listening);
    }

    #[test]
    fn release_alone_does_not_resume_the_chunker_feed() {
        // The realistic race this guards: release fires immediately, but the recognizer's
        // final/error callback can arrive tens to hundreds of ms later — the feed must stay
        // paused across that entire gap, not reopen the instant the button is lifted.
        let mut gate = PushToTalkGate::new();
        gate.on_ptt_pressed();
        gate.on_ptt_released();
        assert_eq!(gate.state(), VoiceState::Listening);
        assert!(gate.is_chunker_paused());
    }

    #[test]
    fn stt_ended_after_release_resumes_the_chunker_feed() {
        let mut gate = PushToTalkGate::new();
        gate.on_ptt_pressed();
        gate.on_ptt_released();
        let actions = gate.on_stt_ended();
        assert_eq!(actions, vec![GateAction::ResumeChunkerFeed]);
        assert_eq!(gate.state(), VoiceState::Idle);
        assert!(!gate.is_chunker_paused());
    }

    #[test]
    fn stt_ended_resumes_on_an_error_outcome_too_not_just_a_clean_final_transcript() {
        // The gate has no notion of "success" vs "error" — any terminal STT outcome must resume.
        let mut gate = PushToTalkGate::new();
        gate.on_ptt_pressed();
        let actions = gate.on_stt_ended();
        assert_eq!(actions, vec![GateAction::ResumeChunkerFeed]);
        assert!(!gate.is_chunker_paused());
    }

    #[test]
    fn stt_ended_while_already_idle_is_a_no_op() {
        let mut gate = PushToTalkGate::new();
        let actions = gate.on_stt_ended();
        assert!(
            actions.is_empty(),
            "a stray/duplicate ended-event with nothing in flight must not emit a spurious resume"
        );
        assert_eq!(gate.state(), VoiceState::Idle);
    }

    #[test]
    fn a_full_press_release_finalize_cycle_leaves_the_gate_ready_for_the_next_round() {
        let mut gate = PushToTalkGate::new();
        gate.on_ptt_pressed();
        gate.on_ptt_released();
        gate.on_stt_ended();
        assert_eq!(gate.state(), VoiceState::Idle);

        // The gate must behave identically on a second full cycle — no leftover state from the
        // first round (e.g. a "half-listening" flag that only the first press could ever set).
        let actions = gate.on_ptt_pressed();
        assert_eq!(
            actions,
            vec![GateAction::StopSpeaking, GateAction::PauseChunkerFeed]
        );
        gate.on_stt_ended();
        assert_eq!(gate.state(), VoiceState::Idle);
    }
}
