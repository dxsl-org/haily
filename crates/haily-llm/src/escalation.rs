//! Escalation policy + egress pin — pure building blocks CONSUMED BY the P4 pipeline
//! runner (which does not exist yet). Nothing here is wired into a live escalation or
//! retry loop this phase; there is no such loop to wire into. Everything is a pure,
//! unit-tested value type so P4 can compose it deterministically.
//!
//! DESIGN (Phase 3 spec §Architecture):
//! - Escalation is `T→T+1` (one [`crate::Tier`] step) ONLY after a concrete count of
//!   verifier failures — never on suspicion. Default OFF (measure-first, honoring the
//!   roadmap's "router A/B gated on eval data" stance).
//! - The step is capped by `max_tier` AND by an [`Egress`] pin: under
//!   [`Egress::LocalOnly`] a would-be escalation to a cloud-served tier is a no-op
//!   (the P4 runner turns that no-op into a pause + one-line consent prompt). This is
//!   the red-team FMA-M2 guard against a silent local→cloud egress change on a run the
//!   user started local.

use crate::Tier;

impl Tier {
    /// Parses the lowercase wire label (`"fast"` | `"medium"` | `"thinking"` | `"ultra"`,
    /// case-insensitive) — the same vocabulary `haily_core::routing::tier_label` serializes to
    /// `routing_decisions.chosen_tier`/`escalated_to` and the `llm.tier_model.<tier>`/
    /// `llm.escalation.max_tier` preference keys use. `None` for anything else (fail-safe:
    /// callers fall back to a sane default rather than erroring on an operator typo).
    pub fn from_name(s: &str) -> Option<Tier> {
        match s.trim().to_lowercase().as_str() {
            "fast" => Some(Tier::Fast),
            "medium" => Some(Tier::Medium),
            "thinking" => Some(Tier::Thinking),
            "ultra" => Some(Tier::Ultra),
            _ => None,
        }
    }
}

/// Network-egress pin for a run/pipeline. `AllowCloud` lets escalation cross to a
/// cloud-served tier; `LocalOnly` caps escalation at the highest locally-served tier so
/// a run the user started local never silently reaches out to the cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Egress {
    /// Escalation must not cross to a cloud-served tier (caps at the highest local tier).
    LocalOnly,
    /// Escalation may cross to a cloud-served tier (subject only to `max_tier`).
    AllowCloud,
}

impl Egress {
    /// Derives the default egress pin from a router's primary-backend locality: a local
    /// `llama.cpp` primary pins `LocalOnly` (escalation must never silently leave the machine
    /// the user started local on); any other primary (cloud) allows `AllowCloud`. Phase 4
    /// (`agent::turn`) and phase 6 (the pipeline runner) both derive egress this same way — an
    /// explicit `llm.escalation.egress` preference override takes precedence over this default
    /// at BOTH call sites and is applied by the caller, not here (this fn is the locality
    /// half only).
    pub fn from_provider(provider_name: &str) -> Egress {
        if provider_name == "llama.cpp" {
            Egress::LocalOnly
        } else {
            Egress::AllowCloud
        }
    }
}

/// Verifier-failure-driven tier escalation policy. Default is OFF with a `Thinking`
/// ceiling — flip `enabled` (and lift `max_tier`) only once P9 eval data justifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscalationPolicy {
    /// Number of consecutive verifier failures at a tier before the next attempt
    /// escalates to `tier + 1`. Counting is the P4 runner's job; this struct only
    /// decides, given a count, whether/where to step.
    pub failures_before_escalation: u32,
    /// Ceiling tier — escalation never steps above this regardless of failure count.
    pub max_tier: Tier,
    /// Master switch. `false` (default) makes every method a no-op — zero behavior
    /// change until an operator opts in.
    pub enabled: bool,
}

impl Default for EscalationPolicy {
    fn default() -> Self {
        Self { failures_before_escalation: 2, max_tier: Tier::Thinking, enabled: false }
    }
}

impl EscalationPolicy {
    /// Compute the tier to attempt next, or `None` for "do not escalate — retry (or
    /// give up) at `current`". Returns `None` when: the policy is disabled; `failures`
    /// has not yet reached `failures_before_escalation`; `current` is already at the
    /// ceiling; or the next tier would cross the egress cap.
    ///
    /// `highest_local_tier` is the top tier the pinned backend serves locally — under
    /// [`Egress::LocalOnly`] the effective ceiling is `min(max_tier, highest_local_tier)`,
    /// so a step that would reach a cloud-only tier resolves to `None` (a no-op the P4
    /// runner surfaces as a consent pause, NOT a silent cloud call).
    ///
    /// NOTE: a `Some(next)` where `next` resolves to the SAME model as `current` (ollama
    /// maps `Thinking`+`Ultra` to one local GGUF) is a routing no-op the RUNNER
    /// short-circuits — this pure function does not know the model map, only tiers.
    pub fn next_tier(
        &self,
        current: Tier,
        failures: u32,
        egress: Egress,
        highest_local_tier: Tier,
    ) -> Option<Tier> {
        if !self.enabled || failures < self.failures_before_escalation {
            return None;
        }
        let ceiling = match egress {
            Egress::AllowCloud => self.max_tier,
            Egress::LocalOnly => self.max_tier.min(highest_local_tier),
        };
        let next = current.next()?;
        (next <= ceiling).then_some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_disabled_and_never_escalates() {
        let p = EscalationPolicy::default();
        assert!(!p.enabled, "escalation must default OFF (measure-first)");
        // Even far past the failure threshold, a disabled policy is a no-op.
        assert_eq!(p.next_tier(Tier::Fast, 99, Egress::AllowCloud, Tier::Ultra), None);
    }

    #[test]
    fn escalates_exactly_after_n_failures() {
        let p = EscalationPolicy { enabled: true, ..Default::default() }; // N = 2
        // Below threshold: no escalation.
        assert_eq!(p.next_tier(Tier::Fast, 0, Egress::AllowCloud, Tier::Ultra), None);
        assert_eq!(p.next_tier(Tier::Fast, 1, Egress::AllowCloud, Tier::Ultra), None);
        // At/above threshold: step exactly one tier up.
        assert_eq!(
            p.next_tier(Tier::Fast, 2, Egress::AllowCloud, Tier::Ultra),
            Some(Tier::Medium)
        );
        assert_eq!(
            p.next_tier(Tier::Fast, 5, Egress::AllowCloud, Tier::Ultra),
            Some(Tier::Medium)
        );
    }

    #[test]
    fn respects_max_tier_ceiling() {
        // Ceiling = Thinking (default). A step that would exceed it is a no-op.
        let p = EscalationPolicy { enabled: true, ..Default::default() };
        assert_eq!(
            p.next_tier(Tier::Medium, 2, Egress::AllowCloud, Tier::Ultra),
            Some(Tier::Thinking)
        );
        // From Thinking the next tier (Ultra) is above max_tier → no-op.
        assert_eq!(p.next_tier(Tier::Thinking, 2, Egress::AllowCloud, Tier::Ultra), None);
    }

    #[test]
    fn ceiling_variant_at_top_returns_none() {
        // Ultra is the ordinal ceiling — there is no tier above it to step to.
        let p = EscalationPolicy { enabled: true, max_tier: Tier::Ultra, ..Default::default() };
        assert_eq!(p.next_tier(Tier::Ultra, 9, Egress::AllowCloud, Tier::Ultra), None);
    }

    #[test]
    fn local_only_caps_at_highest_local_tier() {
        // max_tier=Ultra would allow Fast→Medium, but LocalOnly with a Fast-only local
        // backend caps the ceiling at Fast → a would-be cloud escalation is a no-op.
        let p = EscalationPolicy { enabled: true, max_tier: Tier::Ultra, ..Default::default() };
        assert_eq!(p.next_tier(Tier::Fast, 2, Egress::LocalOnly, Tier::Fast), None);
        // Same policy under AllowCloud DOES escalate — proving the pin is what blocks it.
        assert_eq!(
            p.next_tier(Tier::Fast, 2, Egress::AllowCloud, Tier::Fast),
            Some(Tier::Medium)
        );
        // LocalOnly with a Medium-capable local backend permits Fast→Medium but not
        // Medium→Thinking (Thinking would be cloud-served).
        assert_eq!(
            p.next_tier(Tier::Fast, 2, Egress::LocalOnly, Tier::Medium),
            Some(Tier::Medium)
        );
        assert_eq!(p.next_tier(Tier::Medium, 2, Egress::LocalOnly, Tier::Medium), None);
    }

    #[test]
    fn tier_from_name_parses_case_insensitively_and_rejects_unknown() {
        assert_eq!(Tier::from_name("fast"), Some(Tier::Fast));
        assert_eq!(Tier::from_name("MEDIUM"), Some(Tier::Medium));
        assert_eq!(Tier::from_name("Thinking"), Some(Tier::Thinking));
        assert_eq!(Tier::from_name(" ultra "), Some(Tier::Ultra));
        assert_eq!(Tier::from_name("nonsense"), None);
        assert_eq!(Tier::from_name(""), None);
    }

    #[test]
    fn egress_from_provider_pins_local_only_for_llama_cpp() {
        assert_eq!(Egress::from_provider("llama.cpp"), Egress::LocalOnly);
        assert_eq!(Egress::from_provider("cloud"), Egress::AllowCloud);
        assert_eq!(Egress::from_provider("unconfigured"), Egress::AllowCloud);
    }
}
