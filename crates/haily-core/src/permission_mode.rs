//! Named permission ladder (Unified Chat UI, D5) — a single toggle modulating whether a
//! `ReversibleWrite`/`IrreversibleWrite` tool call auto-runs or waits for an interactive
//! approval. Orthogonal to the `safety.disable_writes` kill switch and the `Blocked` tier:
//! neither is ever modulated by `ApprovalMode` (see `should_prompt`'s exhaustive match and
//! `tool_call::dispatch`'s kill-switch check, which always runs first).
//!
//! The live handle mirrors the existing `safety.disable_writes`/`llm.routing_enabled`
//! pattern (`Arc<AtomicBool>`, threaded through `TurnRuntime`/`SubTurnRequest` and read at
//! dispatch time) rather than a DB row read per turn — a DB-only pref would not affect an
//! in-flight turn. `ApprovalMode` needs 3 states, so the handle is an `AtomicU8` encoding
//! instead of a bool; no new crate dependency is pulled in for a 3-state atomic.

use haily_tools::RiskTier;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// The three named rungs (D5). Ship default is [`ApprovalMode::Manual`] — an unset/missing
/// `approval.mode` preference reads as `Manual`, the strictest rung, even though it is
/// STRICTER than the pre-ladder runtime (where a plain `ReversibleWrite` auto-ran with no
/// prompt). This is a deliberate safe-by-default choice, not a behavior-preserving one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    /// Prompt before every `ReversibleWrite` AND `IrreversibleWrite`.
    #[default]
    Manual,
    /// Auto-run `ReversibleWrite` (today's pre-ladder behavior); still prompt
    /// `IrreversibleWrite`.
    AcceptEdits,
    /// Auto-run everything, including `IrreversibleWrite`. Every auto-approved
    /// `IrreversibleWrite` gets a RECORD-ONLY journal row (audit trail, no compensator, no
    /// undo promised) — see `tool_call::dispatch`'s `auto`-mode branch.
    Auto,
}

impl ApprovalMode {
    /// Parses a persisted preference value. Fail-safe: any string other than the two
    /// looser rungs' exact labels (including empty/unknown/missing) resolves to `Manual` —
    /// the strictest rung — so a corrupted or unrecognized pref value can never silently
    /// unlock a looser write posture.
    pub fn parse(s: &str) -> Self {
        match s {
            "accept_edits" => ApprovalMode::AcceptEdits,
            "auto" => ApprovalMode::Auto,
            _ => ApprovalMode::Manual,
        }
    }

    /// The persisted/wire label — round-trips through [`Self::parse`].
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalMode::Manual => "manual",
            ApprovalMode::AcceptEdits => "accept_edits",
            ApprovalMode::Auto => "auto",
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            ApprovalMode::Manual => 0,
            ApprovalMode::AcceptEdits => 1,
            ApprovalMode::Auto => 2,
        }
    }

    /// Any encoding outside the 3 known values (impossible via `to_u8`, but the atomic cell
    /// is untyped storage) falls back to `Manual` — the same fail-closed rule as `parse`.
    fn from_u8(v: u8) -> Self {
        match v {
            1 => ApprovalMode::AcceptEdits,
            2 => ApprovalMode::Auto,
            _ => ApprovalMode::Manual,
        }
    }
}

/// True if `tier` must wait for an interactive approval under `mode`. `Read` never prompts
/// in any mode; `Blocked` never reaches this function (`dispatch` refuses it before
/// consulting the ladder) but is included so the match stays exhaustive — a future
/// `RiskTier` variant fails to COMPILE here rather than silently falling into an auto-run
/// arm (mirrors the fail-closed contract on `RiskTier` itself).
pub fn should_prompt(mode: ApprovalMode, tier: RiskTier) -> bool {
    match (mode, tier) {
        (_, RiskTier::Read) => false,
        (_, RiskTier::Blocked) => false,
        (ApprovalMode::Manual, RiskTier::ReversibleWrite) => true,
        (ApprovalMode::Manual, RiskTier::IrreversibleWrite) => true,
        (ApprovalMode::AcceptEdits, RiskTier::ReversibleWrite) => false,
        (ApprovalMode::AcceptEdits, RiskTier::IrreversibleWrite) => true,
        (ApprovalMode::Auto, RiskTier::ReversibleWrite) => false,
        (ApprovalMode::Auto, RiskTier::IrreversibleWrite) => false,
    }
}

/// Live handle for the current `approval.mode` — mirrors `Arc<AtomicBool>` (the kill
/// switch): one instance is threaded from `Orchestrator` through every `TurnRuntime`/
/// `SubTurnRequest`/`DelegateTool`/`PipelineRunner`/`JudgeContext`, so a mode change is
/// observed by the very next tool-call dispatch at any depth, in any chat turn or pipeline
/// stage, with no restart.
pub type ApprovalModeHandle = Arc<AtomicU8>;

/// Build a fresh handle seeded to `mode` — the boot-time constructor (mirrors
/// `Arc::new(AtomicBool::new(disable_writes))`).
pub fn new_handle(mode: ApprovalMode) -> ApprovalModeHandle {
    Arc::new(AtomicU8::new(mode.to_u8()))
}

/// Read the current mode. `Acquire` pairs with the `Release` store in [`store`] so a live
/// flip from `set_approval_mode` is observed without a restart (mirrors the kill switch's
/// `Acquire`/`Release` pairing in `tool_call::dispatch`).
pub fn load(handle: &ApprovalModeHandle) -> ApprovalMode {
    ApprovalMode::from_u8(handle.load(Ordering::Acquire))
}

/// Flip the live handle. The caller (`set_approval_mode`) MUST persist the DB preference
/// row BEFORE calling this — a crash between the two then leaves the persisted state no
/// looser than the (now-reverted-on-reboot) running state, i.e. fails toward the stricter
/// pairing, never toward a looser one that silently outlives a crash.
pub fn store(handle: &ApprovalModeHandle, mode: ApprovalMode) {
    handle.store(mode.to_u8(), Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_through_as_str() {
        for mode in [
            ApprovalMode::Manual,
            ApprovalMode::AcceptEdits,
            ApprovalMode::Auto,
        ] {
            assert_eq!(ApprovalMode::parse(mode.as_str()), mode);
        }
    }

    #[test]
    fn parse_unknown_or_empty_fails_safe_to_manual() {
        assert_eq!(ApprovalMode::parse(""), ApprovalMode::Manual);
        assert_eq!(ApprovalMode::parse("bogus"), ApprovalMode::Manual);
        assert_eq!(ApprovalMode::parse("Auto"), ApprovalMode::Manual); // case-sensitive, fails safe
    }

    #[test]
    fn default_is_manual() {
        assert_eq!(ApprovalMode::default(), ApprovalMode::Manual);
    }

    /// Exhaustive table over every (mode × tier) pair — the Success Criteria's ladder
    /// boundary, verified directly rather than via `dispatch` integration tests alone.
    #[test]
    fn should_prompt_table() {
        use ApprovalMode::*;
        use RiskTier::*;
        let cases = [
            (Manual, Read, false),
            (Manual, ReversibleWrite, true),
            (Manual, IrreversibleWrite, true),
            (Manual, Blocked, false),
            (AcceptEdits, Read, false),
            (AcceptEdits, ReversibleWrite, false),
            (AcceptEdits, IrreversibleWrite, true),
            (AcceptEdits, Blocked, false),
            (Auto, Read, false),
            (Auto, ReversibleWrite, false),
            (Auto, IrreversibleWrite, false),
            (Auto, Blocked, false),
        ];
        for (mode, tier, expected) in cases {
            assert_eq!(
                should_prompt(mode, tier),
                expected,
                "should_prompt({mode:?}, {tier:?}) expected {expected}"
            );
        }
    }

    #[test]
    fn handle_load_store_round_trips() {
        let h = new_handle(ApprovalMode::Manual);
        assert_eq!(load(&h), ApprovalMode::Manual);
        store(&h, ApprovalMode::Auto);
        assert_eq!(load(&h), ApprovalMode::Auto);
        store(&h, ApprovalMode::AcceptEdits);
        assert_eq!(load(&h), ApprovalMode::AcceptEdits);
    }
}
