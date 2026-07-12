//! Stealth browser tool surface (Phase 13) — capability-preservation port from haily.go, brought
//! under the current Rust harness (RiskTier + ApprovalGate + P0 network-allowed sandbox profile).
//!
//! # Layering (feature-independent vs `browser`-gated)
//! The DEFAULT workspace build compiles + tests everything that is pure data/logic:
//! [`stealth`] (JS asset, CloakBrowser seam, launch flags), [`human`] (Bézier/typo/timing math),
//! [`fetch_strategy`] (bot/human keyword tables + escalation), and [`session`]'s `SameSite`
//! mapping. Only the LIVE CDP driver ([`manager`]) and the tools that drive it (`tools.rs`) are
//! behind the `browser` cargo feature (default OFF, mirroring the `llama` feature) — so
//! `cargo test --workspace` needs no Chromium.
//!
//! # Security scope (owner's-own-sessions, single interactive session)
//! This automates the browsing the USER already does, with the USER's own logins; mutations are
//! approval-gated. The stealth layer is anti-detection for that one session so it is not blocked
//! as "a bot" — it is NOT proxy/UA rotation, NOT scale scraping, NOT mass account creation, and
//! NOT a credential attack. There is deliberately NO batch/multi-target entry point anywhere in
//! this module: the surface is exactly `browser_navigate` (read), `browser_interact`
//! (mutation, approval-gated), `browser_session` (cookies), plus the single-URL `fetch_strategy`
//! wrapper on `url_fetch`.

pub mod fetch_strategy;
pub mod human;
pub mod session;
pub mod stealth;

#[cfg(feature = "browser")]
pub mod manager;
#[cfg(feature = "browser")]
mod tools;

#[cfg(feature = "browser")]
pub use tools::{BrowserInteractTool, BrowserNavigateTool, BrowserSessionTool};

/// Tool names this module registers under the `browser` feature. Kept as a const (compiled
/// unconditionally) so the domain whitelist in `haily-core::domains` and its wiring test can name
/// them without a cross-crate feature dependency — they resolve in `build_v1` only when the
/// feature is on (exactly the connector-op inert pattern), and are skipped by `sub_registry`
/// otherwise.
pub const BROWSER_TOOL_NAMES: &[&str] =
    &["browser_navigate", "browser_interact", "browser_session"];

use crate::RiskTier;

/// `browser_interact` actions that only READ / render the page (no form submission, no arbitrary
/// JS, no file egress) and so run at [`RiskTier::Read`] without an approval prompt. Everything
/// else (click / fill / eval / pdf) is a mutation gated below.
const INTERACT_READ_ACTIONS: &[&str] =
    &["navigate", "scroll", "surf", "screenshot", "snap", "content", "close"];

/// Classify a `browser_interact` call's blast radius from its `action`. Read-only actions run at
/// [`RiskTier::Read`]; page MUTATIONS (`click`/`fill`/`eval`/`pdf`) return
/// [`RiskTier::IrreversibleWrite`] so they route through the `ApprovalGate` — a browser mutation
/// has NO journal/undo compensator, so it can never be the auto-running `ReversibleWrite` tier.
/// Fail-closed: an absent or unrecognized action is treated as a mutation (blast radius unknown),
/// matching the `RiskTier` fail-closed contract.
pub fn interact_risk_tier(action: Option<&str>) -> RiskTier {
    match action {
        Some(a) if INTERACT_READ_ACTIONS.contains(&a) => RiskTier::Read,
        // click / fill / eval / pdf — and anything unknown/absent — are gated writes.
        _ => RiskTier::IrreversibleWrite,
    }
}

/// `browser_session` actions that only READ cookie state (`list`/`export`) run at
/// [`RiskTier::Read`]. `import` (installs cookies — could hijack a session) and `clear` (forces
/// re-login) MUTATE session state with no undo compensator, so they return
/// [`RiskTier::IrreversibleWrite`] → `ApprovalGate`. Fail-closed on unknown/absent.
pub fn session_risk_tier(action: Option<&str>) -> RiskTier {
    match action {
        Some("list") | Some("export") => RiskTier::Read,
        _ => RiskTier::IrreversibleWrite,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_tool_names_are_exactly_the_three_single_session_tools() {
        // SECURITY: the tool surface is fixed at three single-session tools. This asserts the
        // ABSENCE of any bulk/multi-target/credential-attack entry point — no name here implies
        // batch scraping, mass account creation, or credential stuffing.
        assert_eq!(
            BROWSER_TOOL_NAMES,
            &["browser_navigate", "browser_interact", "browser_session"]
        );
    }

    #[test]
    fn interact_read_actions_do_not_require_approval() {
        for a in INTERACT_READ_ACTIONS {
            assert_eq!(interact_risk_tier(Some(a)), RiskTier::Read, "action {a} should be Read");
        }
    }

    #[test]
    fn interact_mutations_require_approval_gate() {
        // A page mutation has no undo compensator → it must be IrreversibleWrite (approval),
        // never the auto-running ReversibleWrite tier.
        for a in ["click", "fill", "eval", "pdf"] {
            assert_eq!(
                interact_risk_tier(Some(a)),
                RiskTier::IrreversibleWrite,
                "mutation {a} must route through the ApprovalGate"
            );
        }
    }

    #[test]
    fn interact_unknown_or_absent_action_fails_closed() {
        assert_eq!(interact_risk_tier(None), RiskTier::IrreversibleWrite);
        assert_eq!(interact_risk_tier(Some("bogus")), RiskTier::IrreversibleWrite);
    }

    #[test]
    fn session_reads_are_free_mutations_gated() {
        assert_eq!(session_risk_tier(Some("list")), RiskTier::Read);
        assert_eq!(session_risk_tier(Some("export")), RiskTier::Read);
        assert_eq!(session_risk_tier(Some("import")), RiskTier::IrreversibleWrite);
        assert_eq!(session_risk_tier(Some("clear")), RiskTier::IrreversibleWrite);
        assert_eq!(session_risk_tier(None), RiskTier::IrreversibleWrite);
    }
}
